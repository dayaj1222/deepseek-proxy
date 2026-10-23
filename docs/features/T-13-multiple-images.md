# T-13 - Multiple images per turn

**Status:** planned · **Effort:** M · **Depends on:** nothing
**Roadmap entry:** [`PLAN.md`](../../PLAN.md) Phase C

## Problem

Attaching two images sends the first, drops the rest, and tells the model how
many it lost. That is the right *failure* behaviour (a hard rejection would wedge
any client that retries the same request) but it is still a silent capability
limit. A vision task that needs two images simply does not work, and the model is
told to ask the user to re-attach them one at a time - a user-visible detour.

## Step zero: probe the upstream

Before any design, answer one question with the mock and a real account (manual,
opt-in): **does DeepSeek accept more than one `file_id` in a single message?**

- **If yes**: send them all, up to `[limits] max_images_per_turn` (default 4) and
  a total byte cap. This is the good outcome and the whole feature.
- **If no**: keep the current behaviour and implement only the fallback - stitch
  up to 4 images into a grid with the `image` crate (feature-gated, off by
  default, because it is a heavy dependency for an unusual case).

The plan is written so the answer changes the *implementation*, not the design.

## Surface

- `[limits] max_images_per_turn` (default 4), `[limits] max_image_bytes_total`.
- Above the image count -> keep the current drop-with-note behaviour; do not
  start rejecting, for the reason above.
- `requests.images_dropped` in `/stats` keeps its meaning: images the model did
  not receive. If the grid fallback is used, 4 images become 1 and the count is 0
  dropped but the README must say the model sees one stitched image, not four.

## Non-negotiable

`images.rs::fetch_remote` and the `is_global*` SSRF rules are **not touched**.
Each remote URL goes through exactly the same checks as today, per URL. A
multi-image change must not become a reason to loosen a security control, and the
SSRF test suite must pass unmodified.

## Tests

- three data-URL images -> all three reach the prompt (or one grid image, in the
  fallback)
- five images with `max_images_per_turn: 4` -> 4 sent, `images_dropped` is 1
- a remote URL pointing at loopback is refused even when it is the third image
- total byte cap refuses the request with 400 and `param: "messages"`
- existing SSRF tests pass unchanged

## Not in scope

Video, PDF pages, image editing, per-image captions.

## Risks

- **SSRF regression** is the top risk: any change here touches the one place a
  user controls an outbound request. Guard: keep every existing test, add only.
- **Context cost**: four images can consume a large part of the model's context.
  The default cap of 4 is a judgement call, documented as a knob.
