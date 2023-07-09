CREATE TYPE irc_message_type AS ENUM (
    'message', 'emote', 'notice'
);

CREATE TABLE IF NOT EXISTS irc_message (
    ts timestamp NOT NULL,
    channel text NOT NULL,
    nick text NOT NULL,
    username text,
    host text,
    message_type irc_message_type NOT NULL,
    message text NOT NULL
);

CREATE INDEX IF NOT EXISTS irc_message_ts_index ON irc_message (ts);
CREATE INDEX IF NOT EXISTS irc_message_channel_index ON irc_message (channel);
CREATE INDEX IF NOT EXISTS irc_message_nick_index ON irc_message (nick);
CREATE INDEX IF NOT EXISTS irc_message_username_index ON irc_message (username);
CREATE INDEX IF NOT EXISTS irc_message_host_index ON irc_message (host);
CREATE INDEX IF NOT EXISTS irc_message_username_combo_index ON irc_message (nick, username, host);
CREATE INDEX IF NOT EXISTS irc_message_message_index ON irc_message USING GIN (to_tsvector('english', message));

CREATE TYPE irc_presence_event_type AS ENUM (
    'join', 'part', 'quit', 'kick'
);

CREATE TABLE IF NOT EXISTS irc_presence (
    ts timestamp NOT NULL,
    channel text NOT NULL,
    event_type irc_presence_event_type NOT NULL,
    nick text NOT NULL,
    username text,
    host text,
    message text
);

CREATE INDEX IF NOT EXISTS irc_presence_ts_index ON irc_presence (ts);
CREATE INDEX IF NOT EXISTS irc_presence_channel_index ON irc_presence (channel);
CREATE INDEX IF NOT EXISTS irc_presence_event_type_index ON irc_presence (event_type);
CREATE INDEX IF NOT EXISTS irc_presence_nick_index ON irc_message (nick);
CREATE INDEX IF NOT EXISTS irc_presence_username_index ON irc_message (username);
CREATE INDEX IF NOT EXISTS irc_presence_host_index ON irc_message (host);
CREATE INDEX IF NOT EXISTS irc_presence_username_combo_index ON irc_message (nick, username, host);

CREATE TABLE IF NOT EXISTS irc_nick_change (
    ts timestamp NOT NULL,
    channel text NOT NULL,
    from_nick text NOT NULL,
    to_nick text NOT NULL,
    username text,
    host text
);

CREATE INDEX IF NOT EXISTS irc_nick_change_ts_index ON irc_nick_change (ts);
CREATE INDEX IF NOT EXISTS irc_nick_change_channel_idnex ON irc_nick_change (channel);
CREATE INDEX IF NOT EXISTS irc_nick_change_from_nick_index ON irc_nick_change (from_nick);
CREATE INDEX IF NOT EXISTS irc_nick_change_to_nick_index ON irc_nick_change (to_nick);
CREATE INDEX IF NOT EXISTS irc_nick_change_username_index ON irc_nick_change (username);
CREATE INDEX IF NOT EXISTS irc_nick_change_host_index ON irc_nick_change (host);
