# Pi extensions

Small extensions that talk to the proxy. Not part of the Rust workspace — pi
loads them directly (it compiles TypeScript through `jiti`, so there is no build
step).

## Install

Pi loads a single file from a user extensions directory, so link it rather than
copying (a copy drifts the moment the repo changes):

```bash
ln -sf "$PWD/pi-ext/deepseek-rebind.ts" ~/.pi/agent/extensions/deepseek-rebind.ts
```

Restart pi, or reload extensions, and `/rebind` appears in the command list.

## `deepseek-rebind.ts`

Adds `/rebind`, which moves the current conversation to another DeepSeek account
on the next turn.

```
/rebind              show whether a move is armed
/rebind next         least-crowded healthy account that is not the current one
/rebind fresh        same account, brand-new remote session (poisoned context)
/rebind a@b.com      that account, refused by the proxy if it is unhealthy
/rebind off          cancel
```

It works by setting `x-deepseek-rs-rebind` on the next provider request and
then clearing itself. The header is the whole protocol; any other client can
send it without this extension.

Two things worth knowing:

- **A command is not a turn.** `/rebind` never reaches the model and never enters
the transcript, so there is nothing to strip on the replay path. That is the
reason it exists instead of a magic string in a message.
- **The armed move is consumed by the next provider request, whatever it is.**
Pi's `before_provider_headers` event does not say which provider it is for, so
if you switch to a non-DeepSeek model between arming and sending, the header is
set there too (harmlessly ignored) and the move does not happen. Re-arm and
send.
