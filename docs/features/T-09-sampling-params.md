# T-09 - Accept-and-ignore sampling parameters, visibly

**Status:** planned · **Effort:** S · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

Most OpenAI clients send `temperature`, `top_p`, `seed`, `user` and the penalty
fields on every request whether or not the user changed them. They are not in
`ChatRequest`, so serde drops them silently today. That works, but it hides the
one case that does not: a caller tuning `temperature` gets identical output from
every value and has no way to learn why.

Silence and refusal are both wrong. The plan is silence with a receipt.

## Surface

- Accept `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `seed`,
  `user`, `logit_bias`, `stop`-adjacent fields without error. They are parsed
  into `ChatRequest` and then ignored by the turn.
- Response header `x-deepseek-rs-ignored: temperature,top_p` listing exactly the
  parameters present in the request that had no effect. Absent when nothing was
  ignored.
- `logprobs: true` -> 400 `unsupported_parameter`, `param: "logprobs"`. This
  cannot be faked: a caller asking for log probabilities is asking for data we do
  not have, and zeros would be a lie a downstream metric would consume.
- `logprobs: false` is accepted, because it is the default and clients send it.

## Design

One `#[derive(Default)] struct IgnoredParams` collects the fields; a single
`fn ignored_names(&self) -> Vec<&'static str>` produces the header value, so the
header and the docs cannot drift. Log the set once per process at `debug`, not
per request - a per-request log line would be noise.

README gets a "Behaviour worth knowing" bullet: sampling parameters are accepted
and have no effect because the upstream does not expose them.

## Tests

- request with `temperature` -> 200 and header contains `temperature`
- request without any -> header absent
- `logprobs: true` -> 400 with `code: "unsupported_parameter"`
- `logprobs: false` -> 200, header does not list it
- header is comma-joined, stable order (asserted, so a diff is visible)

## Not in scope

Forwarding any of it upstream (the upstream does not accept them). Emulating
`seed` determinism, which is impossible without controlling the model.

## Risks

- **A caller believes `temperature` works** because it is accepted. That is why
  the header exists; the README bullet is the second half of the mitigation.
- Adding a field to `IgnoredParams` without adding it to `ignored_names` would
  silently do nothing. A test iterates the struct's names via a const array used
  by both.
