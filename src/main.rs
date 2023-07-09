use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use color_eyre::Result;
use eyre::{eyre, WrapErr};
use futures::FutureExt;
use lazy_static::lazy_static;
use postgres_types::{FromSql, ToSql};
use regex::Regex;
use std::path::Path;
use std::{env, thread};
use tokio::fs::{self, DirEntry, File};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_postgres::Statement;
use tracing::{info, info_span, trace, warn, Instrument};

fn initialize_logging() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    color_eyre::install().unwrap();

    let fmt_layer = fmt::layer();
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    initialize_logging();

    // exit on panic
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let Some(log_dir) = env::args().skip(1).next() else {
        eyre::bail!("needs required argument: log directory");
    };

    let channel = Path::new(&log_dir)
        .file_name()
        .ok_or_else(|| eyre!("cannot get basename of log directory"))?
        .to_str()
        .ok_or_else(|| eyre!("log directory is not valid unicode"))?
        .to_owned();

    let parallel = thread::available_parallelism()
        .wrap_err("cannot determine parallelism")?
        .get();

    info!("detected {} processors", parallel);

    // postgresql configuration setup
    let pg_config = env::var("PG_CONFIG")
        .wrap_err("PG_CONFIG must be provided")?
        .parse::<tokio_postgres::Config>()
        .wrap_err("cannot parse PG_CONFIG as postgresql configuration string")?;

    let (send, recv) = async_channel::bounded::<DirEntry>(parallel * 2);

    let mut tasks = Vec::new();

    // directory reader
    tasks.push(tokio::spawn(
        async {
            let log_dir = log_dir;
            let send = send;
            let mut readdir = fs::read_dir(&log_dir)
                .await
                .wrap_err_with(|| format!("unable to read directory: {}", &log_dir))?;
            while let Some(dirent) = readdir.next_entry().await? {
                if !dirent.file_type().await?.is_file() {
                    warn!("directory entry not a file: {:?}", dirent.file_name());
                }

                trace!("queueing file: {:?}", dirent.file_name());
                send.send(dirent).await.wrap_err("cannot enqueue file")?;
            }

            Ok(())
        }
        .instrument(info_span!("directory reader task"))
        .map(|r: Result<()>| r.unwrap()),
    ));

    // parser/uploader
    for i in 0..parallel {
        let pg_config = pg_config.clone();
        let recv = recv.clone();

        tasks.push(tokio::spawn(
            file_reader_worker(channel.clone(), pg_config, recv)
                .instrument(info_span!("file reader task", i))
                .map(|r: Result<()>| r.unwrap()),
        ));
    }

    futures::future::join_all(tasks).await;
    Ok(())
}

struct SqlPreparedStatements {
    pub insert_presence: Statement,
    pub insert_nick_change: Statement,
    pub insert_message: Statement,
}

#[derive(Debug, FromSql, ToSql)]
#[postgres(name = "irc_presence_event_type")]
enum IrcPresenceType {
    #[postgres(name = "join")]
    Join,
    #[postgres(name = "part")]
    Part,
    #[postgres(name = "quit")]
    Quit,
    #[postgres(name = "kick")]
    Kick,
}

#[derive(Debug, FromSql, ToSql)]
#[postgres(name = "irc_message_type")]
enum IrcMessageType {
    #[postgres(name = "message")]
    Message,
    #[postgres(name = "emote")]
    Emote,
    #[postgres(name = "notice")]
    Notice,
}

async fn file_reader_worker(
    channel: String,
    pg_config: tokio_postgres::Config,
    path_recv: async_channel::Receiver<DirEntry>,
) -> Result<()> {
    lazy_static! {
        static ref LOG_NAME_REGEX: Regex = Regex::new(r"^(\d{4})-(\d{2})-(\d{2})\.log$").unwrap();
    }

    let (mut db_client, db_conn) = pg_config.connect(tokio_postgres::NoTls).await?;
    tokio::spawn(
        async move { db_conn.await.wrap_err("database connection failed") }
            .instrument(info_span!("database connection worker"))
            .map(|r: Result<()>| r.unwrap()),
    );
    trace!("postgresql connected");

    let prepared = SqlPreparedStatements {
        insert_presence: db_client
            .prepare(concat!(
            "INSERT INTO irc_presence (ts, channel, event_type, nick, username, host, message) ",
            "VALUES ($1, $2, $3, $4, $5, $6, $7)"
        ))
            .await
            .wrap_err("failed to prepare query insert_presence")?,
        insert_nick_change: db_client
            .prepare(concat!(
                "INSERT INTO irc_nick_change (ts, channel, from_nick, to_nick) ",
                "VALUES ($1, $2, $3, $4)"
            ))
            .await
            .wrap_err("failed to prepare query insert_nick_change")?,
        insert_message: db_client
            .prepare(concat!(
                "INSERT INTO irc_message (ts, channel, nick, message_type, message) ",
                "VALUES ($1, $2, $3, $4, $5)"
            ))
            .await
            .wrap_err("failed to prepare query insert_message")?,
    };

    while let Ok(dirent) = path_recv.recv().await {
        let Ok(file_name) = dirent.file_name().into_string() else {
            warn!("filename is not valid unicode: {:?}", dirent.file_name());
            continue;
        };
        async {
            trace!("processing file: {:?}", file_name);
            let Some(captures) = LOG_NAME_REGEX.captures(&file_name) else {
                warn!("file did not match pattern: {:?}", file_name);
                return Result::<()>::Ok(());
            };
            let year: i32 = captures[1].parse().unwrap();
            let month: u32 = captures[2].parse().unwrap();
            let day: u32 = captures[3].parse().unwrap();
            let file_date = NaiveDate::from_ymd_opt(year, month, day).unwrap();
            trace!("file date: {:?}", file_date);

            let file = File::open(dirent.path()).await.wrap_err("open log file")?;
            let mut reader = BufReader::new(file);
            let mut transaction = db_client
                .transaction()
                .await
                .wrap_err("begin transaction")?;

            let mut line_buf = Vec::new();
            loop {
                let bytes_read = reader
                    .read_until(b'\n', &mut line_buf)
                    .await
                    .wrap_err("read log line")?;
                if bytes_read == 0 {
                    // EOF
                    break;
                }

                let line_owned = String::from_utf8_lossy(&line_buf);
                let line = if let Some(trimmed) = line_owned.strip_suffix('\n') {
                    trimmed
                } else {
                    line_owned.as_ref()
                };

                process_log_line(&channel, &mut transaction, &prepared, &file_date, &line)
                    .await
                    .wrap_err("parse line")?;

                line_buf.clear();
            }

            transaction.commit().await.wrap_err("commit transaction")?;
            info!("file done: {:?}", file_name);
            Ok(())
        }
        .instrument(info_span!("file", file_name))
        .await?;
    }
    Ok(())
}

async fn process_log_line(
    channel: &String,
    transaction: &mut tokio_postgres::Transaction<'_>,
    prepared: &SqlPreparedStatements,
    file_date: &NaiveDate,
    line: &str,
) -> Result<()> {
    lazy_static! {
        // hh, mm, ss
        static ref TIME_REGEX: Regex = Regex::new(r"^\[(\d{2}):(\d{2}):(\d{2})\]$").unwrap();
        // type, nick, user, host, reason?
        static ref PRESENCE_MESSAGE_REGEX: Regex = Regex::new(r"^(Joins|Parts|Quits): (\S+) \((\S+)@(\S+)\)(?: \((.*)\))?$").unwrap();
        // old_nick, new_nick
        static ref NICK_CHANGE_REGEX: Regex = Regex::new(r"^(\S+) is now known as (\S+)$").unwrap();
    }

    if line.len() == 0 {
        // empty line, probably at the end
        return Ok(());
    }
    trace!("a line! {}", line);

    let Some((msg_timestamp, rest)) = line.split_once(' ') else {
        warn!("parse failed (missing space): {:?}", line);
        return Ok(());
    };
    let Some(msg_timestamp_parsed) = TIME_REGEX.captures(msg_timestamp) else {
        warn!("parse failed (timestamp format mismatch): {:?}", line);
        return Ok(());
    };
    let hour: u32 = msg_timestamp_parsed[1].parse().unwrap();
    let minute: u32 = msg_timestamp_parsed[2].parse().unwrap();
    let second: u32 = msg_timestamp_parsed[3].parse().unwrap();
    let message_time = NaiveTime::from_hms_opt(hour, minute, second).unwrap();
    let message_ts = NaiveDateTime::new(file_date.clone(), message_time);
    trace!("message timestamp: {:?}", message_ts);

    match rest.chars().nth(0) {
        Some('*') => {
            let Some((stars, system_message)) = rest.split_once(' ') else {
                warn!("parse failed (nothing after stars): {:?}", line);
                return Ok(());
            };
            if stars == "*" {
                // /me
                let Some((nick, message)) = system_message.split_once(' ') else {
                    warn!("parse failed (nothing after nick): {:?}", line);
                    return Ok(());
                };

                let message_type = IrcMessageType::Emote;
                trace!(%message_ts, ?message_type, nick, line = message, "message");

                transaction
                    .query(
                        &prepared.insert_message,
                        &[&message_ts, channel, &nick, &message_type, &message],
                    )
                    .await
                    .wrap_err("failed to insert into irc_message")?;
            } else if stars == "***" {
                // system message
                if let Some(parsed) = PRESENCE_MESSAGE_REGEX.captures(system_message) {
                    let event_type = match &parsed[1] {
                        "Joins" => IrcPresenceType::Join,
                        "Parts" => IrcPresenceType::Part,
                        "Quits" => IrcPresenceType::Quit,
                        _ => unreachable!("invalid regex match"),
                    };
                    let nick = &parsed[2];
                    let user = &parsed[3];
                    let host = &parsed[4];
                    let reason = parsed.get(5).map(|s| s.as_str());

                    trace!(%message_ts, ?event_type, nick, user, host, reason, "presence");

                    transaction
                        .query(
                            &prepared.insert_presence,
                            &[
                                &message_ts,
                                channel,
                                &event_type,
                                &nick,
                                &user,
                                &host,
                                &reason,
                            ],
                        )
                        .await
                        .wrap_err("failed insert into irc_presence")?;
                } else if let Some(parsed) = NICK_CHANGE_REGEX.captures(system_message) {
                    let old_nick = &parsed[1];
                    let new_nick = &parsed[2];

                    trace!(%message_ts, old_nick, new_nick, "nick change");

                    transaction
                        .query(
                            &prepared.insert_nick_change,
                            &[&message_ts, channel, &old_nick, &new_nick],
                        )
                        .await
                        .wrap_err("failed to insert into irc_nick_change")?;
                } else {
                    trace!("ignoring other message: {:?}", system_message);
                };
            } else {
                warn!("parse failed (bad stars): {:?}", line);
                return Ok(());
            }
            Ok(())
        }
        Some(type_char @ ('<' | '-')) => {
            // normal message or notice
            let message_type = match type_char {
                '<' => IrcMessageType::Message,
                '-' => IrcMessageType::Notice,
                _ => unreachable!(),
            };
            let close_char = match type_char {
                '<' => '>',
                '-' => '-',
                _ => unreachable!(),
            };

            let rest = &rest[1..];
            let Some((nick, message)) = rest.split_once(close_char) else {
                warn!("parse failed (no matching pair for nick): {:?}", line);
                return Ok(());
            };
            let Some(message) = message.get(1..) else {
                warn!("parse failed (nothing after nick): {:?}", line);
                return Ok(());
            };

            trace!(%message_ts, ?message_type, nick, line = message, "message");

            transaction
                .query(
                    &prepared.insert_message,
                    &[&message_ts, channel, &nick, &message_type, &message],
                )
                .await
                .wrap_err("failed to insert into irc_message")?;
            Ok(())
        }
        Some(_) => {
            warn!("parse failed (unknown start character): {:?}", line);
            Ok(())
        }
        None => {
            warn!("parse failed (nothing after timestamp): {:?}", line);
            Ok(())
        }
    }
}
