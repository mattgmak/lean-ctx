import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Text, truncateToWidth } from "@earendil-works/pi-tui";
import { Type } from "typebox";
import type { McpBridge } from "./mcp-bridge.js";

// A background job runs inside the lean-ctx MCP bridge process, so it keeps the
// shell allowlist, the compression engine and the job table. The bridge has no
// callback for job completion, so this polls its status action and wakes Pi
// with a follow-up message when a job reaches a terminal state.
const POLL_MS = 2000;
const MAX_TRACKED_JOBS = 8;
const WIDGET_KEY = "lean-ctx-jobs";
const MAX_WIDGET_JOBS = 3;
const LAST_LINE_MAX = 60;
const MAX_WAKE_MESSAGES = 10;
const POLL_TEXT_MAX = 4000;
const STARTED = /\[(?:background|auto-background):([A-Za-z0-9_]+) started/;
const STILL_RUNNING = /\[background:[A-Za-z0-9_]+ running\]/;

interface Job {
	id: string;
	command: string;
	timer: ReturnType<typeof setInterval>;
	wake: RegExp | undefined;
	wakes: number;
	polls: number;
	startedAt: number;
	lastLine: string;
}

type RegisterTool = (def: unknown) => void;

interface Deps {
	pi: ExtensionAPI;
	registerTool: RegisterTool;
	getBridge: () => McpBridge | undefined;
}

const TEXT = Type.Object({
	command: Type.Optional(Type.String({ description: "Shell command to run in the background" })),
	background_action: Type.Optional(
		Type.Union([Type.Literal("status"), Type.Literal("cancel")], {
			description: "Inspect or cancel a job instead of starting one",
		}),
	),
	job_id: Type.Optional(Type.String({ description: "Job id returned when the job started" })),
	cwd: Type.Optional(Type.String({ description: "Working directory (defaults to the session cwd)" })),
	timeout_ms: Type.Optional(
		Type.Number({ description: "Job lifetime in ms (max 3600000). The job keeps running past this and then fails." }),
	),
	wake_on: Type.Optional(
		Type.String({ description: "Regex; wake you while the job still runs when an output line matches, e.g. 'listening on|epoch \\d+'" }),
	),
});

function textOf(result: { content: Array<{ text?: string }> }): string {
	return result.content
		.map((part) => part.text ?? "")
		.join("\n")
		.trim();
}

function clip(text: string): string {
	return text.length > POLL_TEXT_MAX ? `${text.slice(0, POLL_TEXT_MAX)}\n[output clipped]` : text;
}

function elapsed(job: Job): string {
	const seconds = Math.floor((Date.now() - job.startedAt) / 1000);
	if (seconds < 60) return `${seconds}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m${String(seconds % 60).padStart(2, "0")}s`;
	return `${Math.floor(minutes / 60)}h${String(minutes % 60).padStart(2, "0")}m`;
}

function tailLine(output: string): string {
	const lines = output
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line && !STILL_RUNNING.test(line));
	const line = lines.at(-1) ?? "";
	return line.length > LAST_LINE_MAX ? `${line.slice(0, LAST_LINE_MAX - 1)}…` : line;
}

function compileWake(source: string | undefined): RegExp | undefined {
	if (!source) return undefined;
	// Models write Python's (?i) prefix, which JavaScript only accepts as a flag.
	const insensitive = source.startsWith("(?i)");
	try {
		return new RegExp(insensitive ? source.slice(4) : source, insensitive ? "i" : "");
	} catch {
		return undefined;
	}
}

export function registerBackgroundTool({ pi, registerTool, getBridge }: Deps): void {
	const jobs = new Map<string, Job>();
	let ui: ExtensionContext["ui"] | undefined;

	// One line per running job above the editor: elapsed time and latest output.
	const showJobs = () => {
		if (!ui) return;
		if (jobs.size === 0) return ui.setWidget(WIDGET_KEY, undefined);
		ui.setWidget(WIDGET_KEY, (tui, theme) => {
			const timer = setInterval(() => tui.requestRender(), 1000);
			return {
				dispose: () => clearInterval(timer),
				invalidate() {},
				render(width: number) {
					const all = [...jobs.values()];
					const lines = all.slice(0, MAX_WIDGET_JOBS).map((job) =>
						truncateToWidth(
							theme.fg("accent", " ⏵ ")
								+ theme.fg("toolTitle", `${job.id} ${job.command.replace(/\s+/g, " ")}`)
								+ theme.fg("dim", ` · ${elapsed(job)}${job.lastLine ? ` · ${job.lastLine}` : ""}`),
							width,
						),
					);
					if (all.length > MAX_WIDGET_JOBS) {
						lines.push(theme.fg("dim", `   +${all.length - MAX_WIDGET_JOBS} more`));
					}
					return lines;
				},
			};
		});
	};

	const stopTracking = (job: Job) => {
		clearInterval(job.timer);
		jobs.delete(job.id);
		showJobs();
	};

	const wake = (line: string, details: Record<string, unknown>) => {
		pi.sendMessage(
			{ customType: "lean-ctx-background", content: line, display: true, details },
			{ triggerTurn: true, deliverAs: "followUp" },
		);
	};

	const poll = async (job: Job) => {
		const bridge = getBridge();
		if (!bridge) {
			stopTracking(job);
			return;
		}
		job.polls += 1;
		let output: string;
		try {
			output = textOf(
				await bridge.callTool("ctx_shell", { background_action: "status", job_id: job.id }),
			);
		} catch {
			// A transient bridge failure must not end tracking: the job is still
			// inside the lean-ctx process and the next tick can reach it again.
			return;
		}
		const done = !STILL_RUNNING.test(output);
		if (!done) {
			job.lastLine = tailLine(output);
			showJobs();
			if (job.wake && job.wakes < MAX_WAKE_MESSAGES && job.wake.test(output)) {
				job.wakes += 1;
				wake(
					`lean-ctx background job ${job.id} matched your wake pattern and still runs.\n$ ${job.command}\n\n${clip(output)}`,
					{ jobId: job.id, kind: "wake" },
				);
			}
			return;
		}
		stopTracking(job);
		wake(
			`lean-ctx background job ${job.id} finished.\n$ ${job.command}\n\n${clip(output)}`,
			{ jobId: job.id, kind: "done" },
		);
	};

	const start = async (
		args: Record<string, unknown>,
	): Promise<{ content: Array<{ type: "text"; text: string }>; details: unknown }> => {
		const bridge = getBridge();
		if (!bridge) {
			return {
				content: [{ type: "text", text: "lean-ctx MCP bridge is not connected, so ctx_bg cannot run. Use ctx_shell instead." }],
				details: undefined,
			};
		}
		const command = String(args.command ?? "").trim();
		if (!command) {
			return { content: [{ type: "text", text: "command is required to start a background job." }], details: undefined };
		}
		const payload: Record<string, unknown> = { command, run_in_background: true };
		if (typeof args.cwd === "string" && args.cwd) payload.cwd = args.cwd;
		if (typeof args.timeout_ms === "number") payload.timeout_ms = args.timeout_ms;

		let output: string;
		try {
			output = textOf(await bridge.callTool("ctx_shell", payload));
		} catch (err) {
			return {
				content: [{ type: "text", text: `ctx_bg failed: ${err instanceof Error ? err.message : String(err)}` }],
				details: undefined,
			};
		}
		const id = STARTED.exec(output)?.[1];
		if (!id) {
			return { content: [{ type: "text", text: `lean-ctx did not return a job id:\n${clip(output)}` }], details: undefined };
		}
		if (jobs.size >= MAX_TRACKED_JOBS) {
			// The job exists in lean-ctx either way; only the wake is skipped.
			return {
				content: [{ type: "text", text: `${output}\n[not tracked for a wake message: ${MAX_TRACKED_JOBS} jobs already tracked]` }],
				details: { jobId: id },
			};
		}
		const job: Job = {
			id,
			command,
			timer: setInterval(() => void poll(job), POLL_MS),
			wake: compileWake(typeof args.wake_on === "string" ? args.wake_on : undefined),
			wakes: 0,
			polls: 0,
			startedAt: Date.now(),
			lastLine: "",
		};
		job.timer.unref?.();
		jobs.set(id, job);
		showJobs();
		return {
			content: [{ type: "text", text: `${output}\n[a follow-up message arrives when it finishes. Continue with other work instead of polling.]` }],
			details: { jobId: id },
		};
	};

	const inspect = async (
		action: "status" | "cancel",
		jobId: string,
	): Promise<{ content: Array<{ type: "text"; text: string }>; details: unknown }> => {
		const bridge = getBridge();
		if (!bridge) {
			return {
				content: [{ type: "text", text: "lean-ctx MCP bridge is not connected, so ctx_bg cannot inspect a job." }],
				details: undefined,
			};
		}
		let output: string;
		try {
			output = textOf(await bridge.callTool("ctx_shell", { background_action: action, job_id: jobId }));
		} catch (err) {
			return {
				content: [{ type: "text", text: `ctx_bg ${action} failed: ${err instanceof Error ? err.message : String(err)}` }],
				details: undefined,
			};
		}
		const job = jobs.get(jobId);
		if (job && !STILL_RUNNING.test(output)) stopTracking(job);
		return { content: [{ type: "text", text: clip(output) }], details: { jobId } };
	};

	registerTool({
		name: "ctx_bg",
		label: "ctx_bg",
		description:
			"Run a shell command as a lean-ctx background job. Returns a job id at once and wakes you with the "
			+ "result when it finishes, so long builds, test suites, downloads and training runs do not block the turn. "
			+ "The command keeps the lean-ctx shell allowlist and compressed output. "
			+ "Use background_action \"status\" or \"cancel\" with a job_id only when you must inspect or stop a job early.",
		promptSnippet: "Run a long shell command in the background and get woken when it finishes",
		promptGuidelines: [
			"Use ctx_bg instead of ctx_shell for commands that may take longer than about 30 seconds: builds, full test suites, installs, large downloads, training runs.",
			"After ctx_bg, continue with other work or end the turn. A follow-up message with the result arrives by itself, so never poll or sleep to wait for a job.",
			"Jobs stop when Pi exits, because they live in the lean-ctx process attached to this session.",
		],
		parameters: TEXT,
		async execute(_toolCallId: string, params: Record<string, unknown>, _signal: AbortSignal) {
			if (params.background_action === "status" || params.background_action === "cancel") {
				const jobId = String(params.job_id ?? "");
				if (!jobId) {
					return { content: [{ type: "text", text: "job_id is required with background_action." }], details: undefined };
				}
				return inspect(params.background_action, jobId);
			}
			return start(params);
		},
	});

	pi.registerMessageRenderer("lean-ctx-background", (message: { content?: unknown }, { expanded }: { expanded: boolean }, theme: any) => {
		const body = typeof message.content === "string" ? message.content : "";
		const [head, ...rest] = body.split("\n");
		const tail = expanded ? rest : rest.slice(0, 4);
		const lines = [theme.fg("toolTitle", `● ${head}`), ...tail.map((line: string) => theme.fg("dim", `  ${line}`))];
		if (!expanded && rest.length > tail.length) lines.push(theme.fg("dim", `  +${rest.length - tail.length} lines`));
		return new Text(lines.join("\n"), 0, 0);
	});

	pi.on("session_start", async (_event, ctx) => {
		ui = ctx.mode === "tui" ? ctx.ui : undefined;
		showJobs();
	});

	pi.on("session_shutdown", async () => {
		for (const job of jobs.values()) clearInterval(job.timer);
		jobs.clear();
		ui?.setWidget(WIDGET_KEY, undefined);
	});
}
