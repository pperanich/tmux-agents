-- The opencode store's shape, as the reader's queries assume it.
--
-- Synthetic, like every other fixture here: the column names, types and the two event indexes are
-- copied from a measured store, and not one row came off a real session. It is committed as SQL
-- rather than as a database because a database is a binary blob nobody can review, and because the
-- schema is the part a reader has to keep working against.
--
-- The trap this file exists to pin: there is no `message.role` column. The role lives inside
-- `message.data`, at `$.role`, so a query written from the transcript spike's "part rows joined to
-- their message for the role" fails with `no such column: m.role`.

create table session (
  id text primary key,
  parent_id text,
  time_created integer not null,
  time_updated integer not null,
  data text not null
);

create table message (
  id text primary key,
  session_id text not null,
  time_created integer not null,
  time_updated integer not null,
  data text not null
);
create index message_session on message (session_id, time_created);

create table part (
  id text primary key,
  message_id text not null,
  session_id text not null,
  time_created integer not null,
  time_updated integer not null,
  data text not null
);
create index part_message on part (message_id, id);

-- The live tail, for sessions written after opencode's event-sourcing cutover. `event_sequence`
-- holds one high-water row per aggregate, which is the cheap "has anything happened" poll.
create table event (
  id text primary key,
  aggregate_id text not null,
  seq integer not null,
  type text not null,
  data text not null
);
create unique index event_aggregate_seq on event (aggregate_id, seq);
create index event_aggregate_type_seq on event (aggregate_id, type, seq);

create table event_sequence (
  aggregate_id text primary key,
  seq integer not null
);
