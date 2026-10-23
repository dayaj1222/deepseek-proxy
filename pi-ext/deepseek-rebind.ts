/**
 * deepseek-rs rebind — arm a one-shot account move for the next turn.
 *
 * `/rebind` runs locally: it sends nothing to the model and writes nothing to
 * the transcript. It arms a flag, and the next provider request carries
 * `x-deepseek-rs-rebind`, which is the entire protocol on the proxy side.
 *
 * This is deliberately a slash command rather than a magic string in a message.
 * A sentinel would live in the history forever, so detection would have to be
 * delta-only (which breaks on a fresh thread, where delta and replay are the
 * same string) and the text would have to be stripped from the replay path as
 * well. A command has none of those problems because it never becomes a
 * message.
 *
 * Install: see ../README.md.
 */
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const HEADER = "x-deepseek-rs-rebind";

/** Targets the proxy understands, plus `off` which never leaves this file. */
const TARGETS = ["next", "fresh", "off"];

function isEmail(value: string): boolean {
	// Deliberately loose: the proxy is the authority on whether the account
	// exists and is healthy. Rejecting a valid address here would be worse than
	// letting the proxy answer 400.
	return value.includes("@") && !value.startsWith("@") && !value.endsWith("@");
}

export default function deepseekRebind(pi: ExtensionAPI) {
	/** The target to apply to the next provider request, or null. */
	let armed: string | null = null;

	pi.registerCommand("rebind", {
		description: "Move this conversation to another DeepSeek account on the next turn",
		getArgumentCompletions: (prefix) => {
			const candidates = [...TARGETS, "<email>"];
			const hits = candidates.filter((c) => c.startsWith(prefix));
			return hits.length > 0 ? hits.map((value) => ({ value, label: value })) : null;
		},
		handler: async (args, ctx) => {
			const target = args.trim();

			if (!target) {
				ctx.ui.notify(
					armed
						? `armed: the next request rebinds to ${armed}`
						: "usage: /rebind next | fresh | <email> | off",
					"info",
				);
				return;
			}

			if (target === "off") {
				if (armed === null) {
					ctx.ui.notify("nothing armed", "info");
					return;
				}
				armed = null;
				ctx.ui.notify("rebind cancelled", "info");
				return;
			}

			if (!TARGETS.includes(target) && !isEmail(target)) {
				ctx.ui.notify(
					`unknown target "${target}" — use next, fresh, off, or an email`,
					"warning",
				);
				return;
			}

			armed = target;
			ctx.ui.notify(
				`rebind armed → ${target}. Your next message moves the thread and replays the transcript.`,
				"info",
			);
		},
	});

	pi.on("before_provider_headers", (event) => {
		if (armed === null) return;
		const target = armed;
		// Consumed here, not in `after_provider_response`: neither event carries a
		// request id, so the only moment that cannot clear the wrong request is
		// the one that applies the header. It also makes the move exactly-once
		// even if a lower layer retries the request.
		armed = null;
		event.headers[HEADER] = target;
	});

	// A pending move must not leak across sessions: switching away from the
	// conversation it was meant for would silently rebind the next one.
	pi.on("session_shutdown", () => {
		armed = null;
	});
}
