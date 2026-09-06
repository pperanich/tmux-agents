# Serve tma over ssh

Give one device read access to your fleet, and the ability to answer a prompt,
over an ssh connection you already trust. This is not "run tma over ssh", which
is about a pane that happens to hold an `ssh` client; that is [Run tma over
ssh](run-tma-over-ssh.md). This page is about `tma serve`, the process that
answers a remote client on the other end of two pipes.

Nothing here opens a port. tma binds no socket, ships no TLS, and holds no
bearer token: sshd authenticates the caller and starts one `tma serve` per
connection as a forced command. What the connection may do afterwards comes from
a record on your machine, not from anything the client says.

## What you need

- sshd running on the host, reachable from the device.
- The device's ssh public key, and its fingerprint.
- tma on `$PATH` for the account the key logs in as.

## 1. Pair the device

Take the fingerprint of the key the device will connect with:

```
$ ssh-keygen -lf ~/phone-key.pub
256 SHA256:0Mn3XQvCJ8pQe1lU6q2ZKq7BhX0tYr9WfSdA3nGkLpM phone (ED25519)
```

The middle field is the id. Record it:

```
$ tma device pair phone --id SHA256:0Mn3XQvCJ8pQe1lU6q2ZKq7BhX0tYr9WfSdA3nGkLpM
paired SHA256:0Mn3XQvCJ8pQe1lU6q2ZKq7BhX0tYr9WfSdA3nGkLpM as phone
  scopes: read, act:answer, act:steer
```

That writes `~/.config/tma/devices.toml`, mode `0600`, by atomic rename. It is
the authorization record: what a connection may do is read from it on every
request.

## 2. Add the forced command

One line in `~/.ssh/authorized_keys`, on the host, for that key:

```
command="tma serve --stdio --device SHA256:0Mn3XQvCJ8pQe1lU6q2ZKq7BhX0tYr9WfSdA3nGkLpM",restrict ssh-ed25519 AAAAC3Nza… phone
```

Three parts, and each earns its place.

`command="…"` replaces whatever the client asks to run. A connection with this
key gets a serve process and cannot get a shell, so the key is scoped to tma by
sshd rather than by tma.

`--device <fp>` is how serve knows which record to read. OpenSSH performs no
token expansion inside `command=`, so the value cannot be derived at connect
time and has to be written here by hand. **It must be the fingerprint of the key
on that same line.** Nothing checks that today, and the failure modes are
ordinary bugs rather than attacks: a mistyped line, a hand-repaired merge
conflict, a restored dotfile backup, or a second line carrying the same key with
a different `--device` (sshd takes the first match).

`restrict` turns off port forwarding, agent forwarding, X11 and pty allocation.
Keep it. A device needs a pipe, not a terminal.

## 3. Dial it

The client runs ssh with no command of its own and speaks NDJSON on the pipe:

```
$ ssh -T -i ~/phone-key you@host
{"schema":1,"id":"1","t":"hello","app":"probe","app_version":"0","device":"SHA256:0Mn3XQvC…"}
{"schema":1,"id":"1","t":"hello","host":"studio","tma_version":"0.5.13","reconcile_interval_ms":2000,"scopes":["read","act:answer","act:steer"]}
```

Paste the first line, and the second comes back. That is the whole handshake, and
it is a usable smoke test: type it by hand once and you know the forced command,
the pairing and the fingerprint all line up. [The remote wire
protocol](../reference/protocol.md) is the rest of the conversation.

If the id is wrong or the pairing is missing you get one frame and an exit:

```
{"schema":1,"id":"0","t":"error","code":"scope-denied","message":"this device is not paired with this host; run `tma device pair` on the host"}
```

## What the scopes mean

| scope | grants | granted at pairing |
|---|---|---|
| `read` | The fleet, transcripts, receipts, and cards rendered but inert. Implicit: every paired device holds it. | yes |
| `act:answer` | `approve`, `deny`, `question_reply`, `question_reject`. | yes |
| `act:steer` | `steer`, `steer_now`, `interrupt`, `deny_with_message`. | yes |
| `act:always` | `approve_always`, whose affirmative answer grants every following action of its class. | **no** |

Widen one from the host and nowhere else:

```
$ tma device grant phone act:always
phone now holds: read, act:answer, act:steer, act:always
```

There is no in-app path to a wider scope, no approval prompt an app can raise,
and no protocol frame that asks for one. A device that dispatches beyond its
grants gets a receipt reading `refused` / `scope-denied`, and the refusal happens
before the slot is claimed and before anything reaches the pane.

Two classes are unreachable at any scope. An **exec** action runs a command
rather than answering a pane, and it is the one action kind that sends no
keystrokes and therefore has no freshness check at all. And an action outside the
table above is refused even if you wrote it yourself, `compact` included: a phone
reaching `/compact` is the agent's own command plane, which is exactly what the
scopes exist to withhold.

`tma device pair --only` pairs a device with `read` and nothing else, for a
tablet you want to watch with and never answer from.

## Revoking

```
$ tma device revoke phone
revoked phone (SHA256:0Mn3XQvC…)
  remove its `command="tma serve --stdio --device SHA256:0Mn3XQvC…"` line from
  ~/.ssh/authorized_keys too, so the next dial is refused by sshd as well
```

The record is the authority and the `authorized_keys` line is the courtesy.
Removing the line stops the **next dial** and nothing else: it does not touch a
serve process already holding its pipes, and a device that simply never hangs up
would keep receiving fleet rows. So the record is what revocation removes, and
every live serve process re-reads the store **per request and per publish**. A
revoked device's next request is refused, its event stream stops, and the process
exits. Removing the line as well is what stops it dialling back.

## What it costs the host

Every serve connection runs its **own** detection cycle: a `capture-pane` per
agent pane and its guarded stamp writes, at the reconcile interval, per
connection. Two devices plus the daemon is three concurrent detection loops on a
6-to-12 pane fleet.

That is why the connection count is capped. Four by default, refused beyond that
with a typed `too-many-connections` error rather than accepted and starved:

```toml
[serve]
max_connections = 4
reconcile_interval_ms = 2000
```

One consequence is worth stating plainly rather than leaving to be discovered: a
`read`-scoped device is not inert on the host. Subscribing runs a cycle, and a
cycle writes pane options. Those writes are server-side guarded so a cycle that
loses a race loses it correctly, but they are not free. What the scope claims is
that the device authorizes nothing on any agent, and that part is exact.

## Hardening, if your sshd allows it

The one gap above is that `--device <fp>` is written by hand and nothing proves
it names the key sshd actually authenticated. The strong fix needs a config
change you may not control:

```
# /etc/ssh/sshd_config
ExposeAuthInfo yes
```

With that set, sshd writes the authenticated key into a file named by
`$SSH_USER_AUTH`, which a wrapper can compare against `--device` before exec'ing
tma. It is documented here as the hardening option rather than the baseline
precisely because it is not yours to set on a shared host.

Keep the shipped [security model](../explanation/security-model.md) in view
while reading that. tma has one boundary and it is your user account: anyone who
can edit `authorized_keys` can already run `tma act`. What the scope model adds
is the first distinction tma has ever drawn *inside* that account, and the
realistic threat to it is a bug in a file you edited, not an attacker.
