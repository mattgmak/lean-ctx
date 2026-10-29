import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Box, Text, truncateToWidth } from "@earendil-works/pi-tui";
import { Type } from "typebox";
import type { McpBridge } from "./mcp-bridge.js";

// A background job runs inside the lean-ctx MCP bridge process, so it keeps the
// shell allowlist, the compression engine and the job table. The bridge has no
// callback for job completion, so this polls its status action and wakes Pi
// with a follow-up message when a job reaches a terminal state.
const POLL_MS = 2000;
const MAX_TRACKED_JOBS = 8;
const WIDGET_KEY = "lean-ctx-jobs";
const WIDGET_BG = "customMessageBg";
const RESULT_BODY_LINES = 6;
const RESULT_BODY_LINES_EXPANDED = 40;
const MESSAGE_BODY_LINES = 4;
const JOBS_CHANNEL = "agent-sesh:background-jobs-running:v1";

// One glyph table for every site, so no renderer hardcodes a character.
export const ICON = {
	clock: "\uF017",
	done: "\uF00C",
	fail: "\uF00D",
	stopped: "\uF05E",
	running: "\uF110",
	id: "\uF292",
};

// The band token of a result block and of a follow-up message follows the
// outcome, so a failed job never wears the success colour.
export type Outcome = "success" | "failure" | "cancelled" | "pending";
const BAND: Record<Outcome, string> = {
	success: "toolSuccessBg",
	failure: "toolErrorBg",
	cancelled: "toolPendingBg",
	pending: "toolPendingBg",
};
const GLYPH: Record<Outcome, { token: string; mark: string; label: string }> = {
	success: { token: "success", mark: ICON.done, label: "completed" },
	failure: { token: "error", mark: ICON.fail, label: "failed" },
	cancelled: { token: "warning", mark: ICON.stopped, label: "cancelled" },
	pending: { token: "warning", mark: ICON.running, label: "running" },
};
// The palette gives every band token one base colour, so the band alone cannot
// tell a finished job from a running one. Each line also wears a bar in the
// outcome colour.
const BAR = "┃ ";

function barred(
	theme: { fg: (token: string, text: string) => string },
	outcome: Outcome,
	line: string,
): string {
	return theme.fg(GLYPH[outcome].token, BAR) + line;
}
// mcp-bridge drops `structuredContent` and `isError`, so no exit code field
// reaches us and the text alone carries the outcome.
const FAILURE_HINTS = [
	"ERROR:",
	"MCP error",
	"could not verify the process table",
	"not found or expired",
];
const EXIT_LINE = /\bexit[= ]\s*(\d+)/;
const RPC_LINE = /\brpc_ms=(\d+)/;
const MAX_WIDGET_JOBS = 3;
// The glyph plus the elapsed value fill this column on every widget row.
const ELAPSED_COL = 6;
const LAST_LINE_MAX = 60;
const MAX_WAKE_MESSAGES = 10;
const POLL_TEXT_MAX = 4000;
const SLOW_POLL_MS = 30000;
const POLL_TEXT_GUARD = 100000;
const RPC_STATUS_TIMEOUT_MS = 15000;
const WALL_MIN_MS = 1000;
const WALL_MAX_MS = 3600000;
const WALL_RETRY_MS = 30000;
const KILL_SLACK_SEC = 10;
const DRY_RUN_TTL_MS = 10 * 60 * 1000;
// darwin ps has no `etimes` column; it prints `etime` as [[dd-]hh:]mm:ss and
// the parser turns that form into seconds, so the stale cut stays the same.
const PS_ARGS = process.platform === "darwin"
	? ["-eo", "pid,ppid,pgid,sid,etime,args"]
	: ["-eo", "pid,ppid,pgid,sid,etimes,args"];
const SCRATCH_ARGS = new Set(["ps", "grep", "pgrep", "awk", "sort", "head", "tail", "tr"]);
// A token taken from one of these names matches every shell or interpreter on
// the machine, so it can never name the job alone.
const INTERPRETERS = new Set([
	"sh", "bash", "zsh", "dash", "ksh", "fish", "env", "python", "python3", "node", "perl", "ruby",
]);
const BRIDGE_HINT = "lean-ctx";
// pi loads extensions through jiti, where `import.meta.url` is not reliable. The
// self filter matches the file name and the tool name instead of an absolute path.
const SELF_HINTS = ["background-tool", "ctx_bg"];
const STARTED = /\[(?:background|auto-background):([A-Za-z0-9_]+) started/;
const STILL_RUNNING = /\[background:[A-Za-z0-9_]+ running\]/;

interface Job {
	id: string;
	name: string;
	command: string;
	timer: ReturnType<typeof setInterval>;
	pollMs: number;
	slow: boolean;
	wallTimer: ReturnType<typeof setTimeout> | undefined;
	wallMs: number;
	wallRetried: boolean;
	note: string;
	wake: RegExp | undefined;
	wakes: number;
	polls: number;
	startedAt: number;
	lastLine: string;
	wokenLines: Set<string>;
	// The pid list of the last dry run, so a confirm can kill only what it showed.
	dryRun: { pids: number[]; token: string; at: number } | undefined;
}

type RegisterTool = (def: unknown) => void;

interface PsRow {
	pid: number;
	ppid: number;
	pgid: number;
	sid: number;
	etimes: number;
	args: string;
}

interface Deps {
	pi: ExtensionAPI;
	registerTool: RegisterTool;
	getBridge: () => McpBridge | undefined;
}

const TEXT = Type.Object({
	command: Type.Optional(Type.String({ description: "Shell command to run in the background" })),
	name: Type.Optional(
		Type.String({ description: "Short label the user sees in the jobs widget, e.g. 'full test suite'" }),
	),
	background_action: Type.Optional(
		Type.Union([
			Type.Literal("status"),
			Type.Literal("cancel"),
			Type.Literal("force_kill"),
		], {
			description: "Inspect, cancel or SIGKILL a job instead of starting one. force_kill needs a job this session tracked",
		}),
	),
	job_id: Type.Optional(Type.String({ description: "Job id returned when the job started" })),
	confirm: Type.Optional(
		Type.Boolean({
			description: "force_kill only: true on the second call kills the rows the dry run listed. Without it nothing is killed",
		}),
	),
	cwd: Type.Optional(Type.String({ description: "Working directory (defaults to the session cwd)" })),
	timeout_ms: Type.Optional(
		Type.Number({ description: "Job lifetime in ms (max 3600000). The job keeps running past this and then fails." }),
	),
	wall_timeout_ms: Type.Optional(
		Type.Number({
			minimum: WALL_MIN_MS,
			maximum: WALL_MAX_MS,
			description: "Wall clock limit in ms (1000..3600000); the job is cancelled when it fires",
		}),
	),
	rpc_timeout_ms: Type.Optional(
		Type.Number({ description: "Bound on one bridge call in ms (default 15000 for status and cancel, 120000 for a start)" }),
	),
	unique: Type.Optional(
		Type.Boolean({ description: "Force a fresh job id for an identical command by adding a random CTXBG_NONCE env value" }),
	),
	env: Type.Optional(
		Type.Record(Type.String(), Type.String(), {
			description: "Extra environment variables for the job. A LEAN_CTX* key is refused and nothing runs.",
		}),
	),
	wake_on: Type.Optional(
		Type.String({ description: "Regex; wake you while the job still runs when an output line matches, e.g. 'listening on|epoch \\d+'" }),
	),
});

function plainWidth(text: string): number {
	return text.replace(/\u001b\[[0-9;]*m/g, "").length;
}

// Fill a line with the theme background. Falls back to plain text on an older
// theme object without a bg helper.
function background(theme: unknown, text: string, token: string = WIDGET_BG): string {
	const bg = (theme as { bg?: (color: string, text: string) => string }).bg;
	return typeof bg === "function" ? bg.call(theme, token, text) : text;
}

// A separator glyph names the element after it, and two spaces hold it away
// from both neighbours.
function field(theme: any, glyph: string, markToken: string, value: string, valueToken = "text"): string {
	return `${theme.fg(markToken, glyph)}  ${theme.fg(valueToken, value)}`;
}

function boldTitle(theme: any, text: string): string {
	const painted = theme.fg("toolTitle", text);
	return typeof theme.bold === "function" ? theme.bold(painted) : painted;
}

function resultText(outcome: Outcome, exitCode?: number): string {
	const label = GLYPH[outcome].label;
	return exitCode === undefined ? label : `${label}, exit ${exitCode}`;
}

export interface HeadFields {
	name: string;
	elapsedMs: number;
	outcome: Outcome;
	exitCode?: number;
	jobId: string;
}

// One line, four fields: name, elapsed, result, job id. The tool result and the
// follow-up message share this shape, so both read the same way.
export function headLine(theme: any, fields: HeadFields): string {
	const glyph = GLYPH[fields.outcome];
	return [
		boldTitle(theme, fields.name),
		field(theme, ICON.clock, "dim", formatElapsed(fields.elapsedMs), "accent"),
		field(theme, glyph.mark, glyph.token, resultText(fields.outcome, fields.exitCode)),
		field(theme, ICON.id, "dim", fields.jobId, "dim"),
	].join("  ");
}

function exitCodeOf(text: string): number | undefined {
	const found = EXIT_LINE.exec(text)?.[1];
	return found === undefined ? undefined : Number(found);
}

// A failure hint and a non-zero exit win over any other signal; a terminal exit
// line of 0 is the only proof of success. Everything else, a running marker
// included, leaves the outcome open.
export function classifyOutcome(text: string, action: string): Outcome {
	if (FAILURE_HINTS.some((hint) => text.includes(hint))) return "failure";
	const code = exitCodeOf(text);
	if (code !== undefined) return terminalOutcome(code);
	// A wall stop names its cancel in words and carries no exit line.
	if (CANCEL_HINTS.some((hint) => text.includes(hint))) return "cancelled";
	// The rules hold for every action, so the text alone decides; `action` stays
	// in the signature because every caller states the request it made.
	void action;
	// A running marker and any other text without an exit line state a job that
	// carries no outcome yet.
	return "pending";
}

// A cancelled job leaves with 130, the shell code for SIGINT, so the bare exit
// code is enough to tell it apart from a failure.
const CANCEL_EXIT = 130;
const CANCEL_HINTS = ["was cancelled", "was stopped"];

function terminalOutcome(code: number): Outcome {
	if (code === 0) return "success";
	return code === CANCEL_EXIT ? "cancelled" : "failure";
}

// The renderers trust the carried outcome and fall back to the text when an
// older message holds no outcome field.
function outcomeOf(details: unknown, text: string): Outcome {
	const stated = (details as { outcome?: unknown } | undefined)?.outcome;
	if (stated === "success" || stated === "failure" || stated === "cancelled" || stated === "pending") return stated;
	return classifyOutcome(text, "");
}

function outcomeFields(text: string, action: string): { outcome: Outcome; exitCode?: number } {
	const exitCode = exitCodeOf(text);
	return exitCode === undefined
		? { outcome: classifyOutcome(text, action) }
		: { outcome: classifyOutcome(text, action), exitCode };
}

function textOf(result: { content: Array<{ text?: string }> }): string {	return result.content
		.map((part) => part.text ?? "")
		.join("\n")
		.trim();
}

function clip(text: string): string {
	return text.length > POLL_TEXT_MAX ? `${text.slice(0, POLL_TEXT_MAX)}\n[output clipped]` : text;
}

// A follow-up message shows its whole body in the chat, so the body is bounded
// twice: the head and the tail keep the shape of the output, and a char cap
// keeps the message small enough to read on one screen.
const MESSAGE_BODY_MAX = 1300;
const MESSAGE_HEAD_LINES = 2;
const MESSAGE_TAIL_LINES = 4;

export function boundMessageBody(output: string): string {
	const lines = output.split("\n");
	if (lines.length <= MESSAGE_HEAD_LINES + MESSAGE_TAIL_LINES) return output;
	let head = lines.slice(0, MESSAGE_HEAD_LINES);
	let tail = lines.slice(-MESSAGE_TAIL_LINES);
	let hidden = lines.length - head.length - tail.length;
	const marker = () => `… ${hidden} lines hidden`;
	const body = () => [...head, marker(), ...tail].join("\n");
	while (body().length > MESSAGE_BODY_MAX) {
		// Whole lines leave the middle of the kept block first, so no line is cut
		// in half and the marker keeps the count of every removed line.
		if (head.length > 1) {
			head = head.slice(0, -1);
			hidden += 1;
			continue;
		}
		if (tail.length > 1) {
			tail = tail.slice(1);
			hidden += 1;
			continue;
		}
		const budget = MESSAGE_BODY_MAX - marker().length - 2;
		const lead = Math.floor(budget / 2);
		return `${head[0].slice(0, lead)}\n${marker()}\n${tail[0].slice(-(budget - lead))}`;
	}
	return body();
}

function formatElapsed(ms: number): string {
	const seconds = Math.max(0, Math.floor(ms / 1000));
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

function messageOf(err: unknown): string {
	return err instanceof Error ? err.message : String(err);
}

// Zero is a value the bridge understands: it disables the bound on that call.
function rpcTimeoutOf(args: Record<string, unknown>): number | undefined {
	const value = args.rpc_timeout_ms;
	return typeof value === "number" && value >= 0 ? value : undefined;
}

// Clamp to the documented window instead of refusing: a job with a slightly
// out-of-range wall limit is still better bounded than unbounded.
function wallMsOf(args: Record<string, unknown>): number {
	const value = args.wall_timeout_ms;
	if (typeof value !== "number" || !Number.isFinite(value)) return 0;
	return Math.min(WALL_MAX_MS, Math.max(WALL_MIN_MS, Math.floor(value)));
}

function basename(word: string): string {
	const cut = word.lastIndexOf("/");
	return cut < 0 ? word : word.slice(cut + 1);
}

// A leading interpreter would match every shell on the machine or the MCP
// bridge itself (`lean-ctx`). Every leading interpreter word goes, so
// `/bin/bash script.sh` and `env /bin/bash x.sh` both settle on the script.
function killToken(command: string): string {
	const words = command.trim().split(/\s+/).filter(Boolean);
	while (words.length > 0) {
		const head = words[0];
		if (head === undefined || !INTERPRETERS.has(basename(head).toLowerCase())) break;
		words.shift();
	}
	return words.find((word) => word.length >= 4 && (word.includes("/") || word.includes("."))) ?? "";
}

// Linux prints whole seconds in `etimes`; darwin prints [[dd-]hh:]mm:ss in
// `etime`. Both land in one second count, so the row parser is one code path.
export function parseElapsed(field: string): number {
	const dash = field.indexOf("-");
	const days = dash < 0 ? 0 : Number(field.slice(0, dash));
	const clock = dash < 0 ? field : field.slice(dash + 1);
	const parts = clock.split(":").map(Number);
	if (!Number.isFinite(days) || parts.length === 0 || parts.length > 3) return Number.NaN;
	let seconds = 0;
	for (const part of parts) {
		if (!Number.isFinite(part)) return Number.NaN;
		seconds = seconds * 60 + part;
	}
	return days * 86400 + seconds;
}

function parsePs(output: string): PsRow[] {
	const rows: PsRow[] = [];
	for (const line of output.split("\n")) {
		const cols = line.trim().split(/\s+/);
		if (cols.length < 6) continue;
		const [pid, ppid, pgid, sid] = cols.slice(0, 4).map(Number);
		const etimes = parseElapsed(cols[4] ?? "");
		if (![pid, ppid, pgid, sid, etimes].every((value) => Number.isFinite(value))) continue;
		rows.push({ pid, ppid, pgid, sid, etimes, args: cols.slice(5).join(" ") });
	}
	return rows;
}

function hasToken(args: string, token: string): boolean {
	if (!token) return false;
	return args.split(/\s+/).some((arg) => arg === token || arg.endsWith(`/${token}`));
}

// Every row in this set is the agent process or one of its ancestors, so a kill
// must never reach it.
function selfChain(rows: PsRow[], selfPid: number): Set<number> {
	const parent = new Map(rows.map((row) => [row.pid, row.ppid]));
	const chain = new Set<number>([selfPid]);
	let pid = selfPid;
	// Bounded walk: a corrupt ppid loop must not hang the kill.
	for (let i = 0; i < 32; i += 1) {
		const next = parent.get(pid);
		if (next === undefined || next <= 1 || chain.has(next)) break;
		chain.add(next);
		pid = next;
	}
	return chain;
}

export interface OwnGroups {
	selfPgid: number | undefined;
	ancestorPgids: Set<number>;
	// False when the table holds no row for this process: the ancestor walk has
	// no start point then, so no group above us is proven safe.
	verifiable: boolean;
}

// `process.getpgrp` is POSIX only and is not part of the Node API, so our own
// group comes from our own row in the table, with the runtime helper first where
// it exists.
type PgrpHost = { getpgrp?: () => number };

function currentPgid(rows: PsRow[], selfPid: number): number | undefined {
	try {
		const host = process as unknown as PgrpHost;
		if (typeof host.getpgrp === "function") return host.getpgrp();
	} catch {
		return undefined;
	}
	return rows.find((row) => row.pid === selfPid)?.pgid;
}

// A group signal on our own group, or on a group an ancestor of pi belongs to,
// reaches pi itself. Those groups are never safe targets.
export function ownGroups(rows: PsRow[], selfPid: number): OwnGroups {
	const byPid = new Map(rows.map((row) => [row.pid, row]));
	const ancestorPgids = new Set<number>();
	// Without our own row the chain walk cannot run at all, so it reports no
	// proven group instead of an empty set that reads as safe.
	const verifiable = byPid.has(selfPid);
	if (verifiable) {
		for (const pid of selfChain(rows, selfPid)) {
			if (pid === selfPid) continue;
			const row = byPid.get(pid);
			if (row) ancestorPgids.add(row.pgid);
		}
	}
	return { selfPgid: currentPgid(rows, selfPid), ancestorPgids, verifiable };
}

// A group leader takes its children with it, so the negative pid clears the
// whole tree. The negative form needs a known own group that differs from the
// row's group; on an unknown own group it could reach pi, so the row keeps its
// positive pid alone.
export function killSignal(row: PsRow, own: OwnGroups): { signal: number; group: boolean } {
	if (own.selfPgid === undefined || own.selfPgid === row.pgid) return { signal: row.pid, group: false };
	if (row.pid === row.pgid && row.pgid > 1) return { signal: -row.pid, group: true };
	return { signal: row.pid, group: false };
}

function pickTargets(
	rows: PsRow[],
	selfPid: number,
	token: string,
	maxElapsedSec: number,
): { targets: PsRow[]; stale: PsRow[]; blocked: PsRow[]; own: OwnGroups } {
	const chain = selfChain(rows, selfPid);
	const own = ownGroups(rows, selfPid);
	const targets: PsRow[] = [];
	const stale: PsRow[] = [];
	const blocked: PsRow[] = [];
	for (const row of rows) {
		if (chain.has(row.pid)) continue;
		// Every direct child of pi is the bridge, a helper of it, or a shell pi
		// spawned itself, so a kill must never reach one.
		if (row.ppid === selfPid) continue;
		if (row.args.includes(BRIDGE_HINT)) continue;
		if (SELF_HINTS.some((hint) => row.args.includes(hint))) continue;
		if (row.args.split(/\s+/).some((arg) => SCRATCH_ARGS.has(arg))) continue;
		if (!hasToken(row.args, token)) continue;
		if (row.etimes > maxElapsedSec) {
			stale.push(row);
			continue;
		}
		if (own.ancestorPgids.has(row.pgid)) {
			blocked.push(row);
			continue;
		}
		targets.push(row);
	}
	return { targets, stale, blocked, own };
}

function runPs(): Promise<string> {
	return new Promise((resolve, reject) => {
		execFile("ps", PS_ARGS, { maxBuffer: 8 * 1024 * 1024 }, (error, stdout) => {
			if (error) reject(error);
			else resolve(stdout);
		});
	});
}

export function psTable(rows: PsRow[], own: OwnGroups): string[] {
	const cell = (value: number | string, width: number): string => String(value).padStart(width);
	const head = `${cell("pid", 7)} ${cell("ppid", 7)} ${cell("pgid", 7)} ${cell("sid", 7)} ${cell("etimes", 6)} ${cell("group", 5)}  args`;
	const lines = rows.map((row) => `${[
		cell(row.pid, 7),
		cell(row.ppid, 7),
		cell(row.pgid, 7),
		cell(row.sid, 7),
		cell(row.etimes, 6),
		// The group column says which signal the row takes: the negative pid, or
		// the positive pid alone. An unknown own group can never earn the negative
		// form, so the reason goes in the same cell.
		cell(
			killSignal(row, own).group ? "yes" : own.selfPgid === undefined ? "no (pgid unknown)" : "no",
			5,
		),
	].join(" ")}  ${row.args}`);
	return [head, ...lines];
}

// A setWidget call deletes the widget key, then re-inserts it. A repeat call
// would move the band to the bottom of its group. The widget therefore takes
// one factory component per session and keeps its lines here. A repaint reads
// this state alone and never the jobs map, so it cannot reorder a poll.
export interface WidgetLine {
	elapsed: string;
	name: string;
	tail: string;
}

export const widgetState: {
	registered: boolean;
	tui: { requestRender(): void } | undefined;
	theme: any;
	lines: WidgetLine[];
	overflow: number;
} = { registered: false, tui: undefined, theme: undefined, lines: [], overflow: 0 };

function resetWidget(): void {
	widgetState.registered = false;
	widgetState.tui = undefined;
	widgetState.theme = undefined;
	widgetState.lines = [];
	widgetState.overflow = 0;
}

function sameLines(left: WidgetLine[], right: WidgetLine[]): boolean {
	if (left.length !== right.length) return false;
	return left.every((line, index) => {
		const other = right[index] as WidgetLine;
		return line.elapsed === other.elapsed && line.name === other.name && line.tail === other.tail;
	});
}

// The elapsed column keeps a fixed width, so the name starts at the same column
// on every row. The pad is a plain space, which keeps the band.
function clockCell(theme: any, elapsed: string): string {
	const room = ELAPSED_COL - plainWidth(ICON.clock);
	return theme.fg("dim", ICON.clock) + theme.fg("accent", elapsed.padStart(room).slice(-room));
}

// Order: elapsed, name, command. The name yields first, so a long label never
// pushes the elapsed column or the command out of the row.
function widgetRow(theme: any, line: WidgetLine, width: number): string {
	const cell = clockCell(theme, line.elapsed);
	const room = width - plainWidth(cell) - 4;
	if (room < 2) return truncateToWidth(cell, width);
	const nameRoom = Math.min(plainWidth(line.name), Math.max(1, Math.floor(room / 2)));
	const name = truncateToWidth(line.name, nameRoom);
	const tailRoom = room - plainWidth(name);
	const tail = tailRoom > 0 ? `  ${theme.fg("muted", truncateToWidth(line.tail, tailRoom))}` : "";
	return truncateToWidth(cell + "  " + theme.fg("toolTitle", name) + tail, width);
}

// Trailing blank cells get trimmed, so a subtle dot fills the band out to the
// terminal edge and keeps the background visible.
function paintRow(theme: any, line: string, width: number): string {
	const pad = Math.max(0, width - plainWidth(line) - 2);
	return background(theme, " " + line + theme.fg("borderMuted", "·".repeat(pad)) + " ");
}

// The widget output is a function of the stored lines alone, so a repaint that
// lands inside a poll shows the set the last rebuild captured.
function widgetRender(theme: any, width: number): string[] {
	if (widgetState.lines.length === 0 && widgetState.overflow === 0) return [];
	const inner = Math.max(1, width - 2);
	const lines = widgetState.lines.map((line) => paintRow(theme, widgetRow(theme, line, inner), width));
	if (widgetState.overflow > 0) {
		lines.push(paintRow(theme, theme.fg("dim", `+${widgetState.overflow} more`), width));
	}
	return lines;
}

export function registerBackgroundTool({ pi, registerTool, getBridge }: Deps): void {
	const jobs = new Map<string, Job>();
	let ui: ExtensionContext["ui"] | undefined;
	let ticker: ReturnType<typeof setInterval> | undefined;
	// Set on session_shutdown, so a late timer cannot reopen the turn.
	let stopped = false;
	resetWidget();

	const stopTicker = () => {
		if (ticker) clearInterval(ticker);
		ticker = undefined;
	};

	// A start, a finish, a cancel and a changed last line rebuild the stored
	// lines. A rebuild that finds the same lines repaints nothing.
	const rebuildWidget = (): void => {
		const all = [...jobs.values()].map((job) => ({
			elapsed: formatElapsed(Date.now() - job.startedAt),
			name: job.name || job.id,
			tail: (job.lastLine || job.command).replace(/\s+/g, " ") + (job.slow ? " · poll 30s" : ""),
		}));
		const lines = all.slice(0, MAX_WIDGET_JOBS);
		const overflow = Math.max(0, all.length - MAX_WIDGET_JOBS);
		if (overflow === widgetState.overflow && sameLines(lines, widgetState.lines)) return;
		widgetState.lines = lines;
		widgetState.overflow = overflow;
		widgetState.tui?.requestRender();
	};

	// The widget keeps one component for the whole session, so this call happens
	// once per session. The ticker refreshes the elapsed cell of that component.
	const showJobs = (): void => {
		// After shutdown the handle is gone, so a late wall retry cannot reopen the
		// widget or start its ticker again.
		if (stopped) return;
		const target = ui;
		if (!target) return;
		if (!widgetState.registered) {
			widgetState.registered = true;
			target.setWidget(WIDGET_KEY, (tui: any, theme: any) => {
				widgetState.tui = tui;
				widgetState.theme = theme;
				return {
					invalidate() {},
					render: (width: number) => widgetRender(widgetState.theme ?? theme, width),
				};
			}, { placement: "aboveEditor" });
		}
		if (jobs.size === 0) stopTicker();
		else if (!ticker) {
			ticker = setInterval(showJobs, 1000);
			ticker.unref?.();
		}
		rebuildWidget();
	};

	// The factory awaits the MCP bridge, so this extension registers its
	// session_start handler after that event fired. A tool call carries the same
	// context, so the first call also adopts the widget target.
	const adoptContext = (ctx?: ExtensionContext): void => {
		if (!ctx) return;
		ui = ctx.mode === "tui" ? ctx.ui : undefined;
		showJobs();
	};

	// The payload carries the live count, so a listener that missed one event
	// still recovers on the next one. A bus failure must not break job tracking.
	const emitJobsActive = (phase: "start" | "stop", job: Job): void => {
		try {
			const bus = pi.events;
			if (typeof bus?.emit !== "function") return;
			bus.emit(JOBS_CHANNEL, {
				jobId: job.id,
				...(job.name ? { name: job.name } : {}),
				phase,
				active: jobs.size,
			});
		} catch {
			// A throwing listener leaves the jobs map alone.
		}
	};

	const stopTracking = (job: Job) => {
		clearInterval(job.timer);
		if (job.wallTimer) clearTimeout(job.wallTimer);
		job.wallTimer = undefined;
		// A dry-run list dies with the tracking it belongs to.
		job.dryRun = undefined;
		jobs.delete(job.id);
		emitJobsActive("stop", job);
		showJobs();
	};

	// The header names the job, so the tool result and the follow-up carry the
	// four fields a renderer needs, and no renderer guesses them.
	const jobHead = (job: Job) => ({
		name: job.name || job.id,
		jobId: job.id,
		elapsedMs: Date.now() - job.startedAt,
	});

	// Every reply ends with the same diagnosis line, so a slow bridge call and a
	// large status payload are visible without a second tool call.
	const reply = (
		body: string,
		log: { started: number; chars: number; jobId?: string },
		details?: unknown,
		tail?: string,
	) => {
		const text = `${body}\nrpc_ms=${Date.now() - log.started} payload_chars=${log.chars} job_id=${log.jobId ?? "-"}`
			+ (tail ? `\n${tail}` : "");
		return {
			content: [{ type: "text" as const, text }],
			details: { ...(details as Record<string, unknown> | undefined), ...outcomeFields(text, "") },
		};
	};

	// A status or kill call without a job id is a caller mistake, not a job.
	const missingJobId = () => reply("job_id is required with background_action.", { started: Date.now(), chars: 0 });

	// The host abort signal reaches the bridge, so a tool call the agent cancelled
	// stops the bridge call too. Both unset keeps the short two-argument call.
	const callBridge = (
		bridge: McpBridge,
		payload: Record<string, unknown>,
		timeoutMs?: number,
		signal?: AbortSignal,
	) => {
		if (timeoutMs === undefined && signal === undefined) return bridge.callTool("ctx_shell", payload);
		return bridge.callTool("ctx_shell", payload, signal, timeoutMs);
	};

	// A status text above the guard gets polled every 30 s: the tail of a chatty
	// job grows on every poll, and the buffer is what has to stay bounded.
	const setPollMs = (job: Job, ms: number) => {
		job.slow = ms !== POLL_MS;
		if (job.pollMs === ms) return;
		job.pollMs = ms;
		clearInterval(job.timer);
		job.timer = setInterval(() => void poll(job), ms);
		job.timer.unref?.();
	};

	const wake = (line: string, details: Record<string, unknown>) => {
		if (stopped) return;
		pi.sendMessage(
			{
				customType: "lean-ctx-background",
				content: line,
				display: true,
				details: { ...details, ...outcomeFields(line, String(details.kind ?? "")) },
			},
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
				await callBridge(
					bridge,
					{ background_action: "status", job_id: job.id },
					RPC_STATUS_TIMEOUT_MS,
				),
			);
		} catch {
			// A transient bridge failure must not end tracking: the job is still
			// inside the lean-ctx process and the next tick can reach it again.
			return;
		}
		setPollMs(job, output.length > POLL_TEXT_GUARD ? SLOW_POLL_MS : POLL_MS);
		const done = !STILL_RUNNING.test(output);
		if (!done) {
			job.lastLine = tailLine(output);
			showJobs();
			if (job.wake && job.wakes < MAX_WAKE_MESSAGES) {
				// Status output repeats earlier lines, so wake only on a matching line
				// that did not wake before.
				const match = output.split("\n").find((line) => job.wake!.test(line) && !job.wokenLines.has(line));
				if (match) {
					job.wokenLines.add(match);
					job.wakes += 1;
					wake(
						`lean-ctx background job ${job.name || job.id} matched your wake pattern and still runs.\n$ ${job.command}\n\n${boundMessageBody(output)}`,
						{ ...jobHead(job), kind: "wake" },
					);
				}
			}
			return;
		}
		stopTracking(job);
		wake(
			`lean-ctx background job ${job.id} finished.\n$ ${job.command}\n\n${boundMessageBody(output)}`,
			{ ...jobHead(job), kind: "done" },
		);
	};

	// The wall timer is the only bound that holds for a job which never stops
	// writing output, where the idle-keyed timeout_ms never fires.
	const wallStop = async (job: Job) => {
		const bridge = getBridge();
		if (bridge) {
			try {
				await callBridge(
					bridge,
					{ background_action: "cancel", job_id: job.id },
					RPC_STATUS_TIMEOUT_MS,
				);
			} catch (err) {
				// A poll tick can settle the job while the cancel is in flight.
				if (!jobs.has(job.id)) return;
				const reason = messageOf(err);
				if (job.wallRetried) {
					// A second failure loses the wall bound for good: the job leaves
					// tracking and the follow-up says the limit did not apply.
					stopTracking(job);
					wake(
						`lean-ctx background job ${job.name || job.id} passed its wall limit of ${job.wallMs}ms `
							+ "and is still running."
							+ `\n[the wall limit could not be applied: the cancel failed twice, so this job is no longer tracked; last error: ${reason}]`
							+ `\n$ ${job.command}`,
						{ ...jobHead(job), kind: "wall" },
					);
					return;
				}
				// A failed cancel keeps the job tracked: one retry in 30 s, then the
				// next poll decides, so a transient bridge failure costs no job.
				job.wallRetried = true;
				job.note = `wall cancel failed: ${reason}; retried once after ${WALL_RETRY_MS / 1000}s`;
				const retry = setTimeout(() => {
					job.wallTimer = undefined;
					void wallStop(job);
				}, WALL_RETRY_MS);
				retry.unref?.();
				job.wallTimer = retry;
				showJobs();
				return;
			}
		}
		// The poll tick can settle the job while the cancel is in flight, so a job
		// that already left tracking wakes no second time.
		if (!jobs.has(job.id)) return;
		stopTracking(job);
		wake(
			`lean-ctx background job ${job.name || job.id} hit its wall limit of ${job.wallMs}ms and was cancelled.`
				+ (job.note ? `\n[${job.note}]` : "")
				+ `\n$ ${job.command}`,
			{ ...jobHead(job), kind: "wall" },
		);
	};

	const start = async (
		args: Record<string, unknown>,
		signal?: AbortSignal,
	): Promise<{ content: Array<{ type: "text"; text: string }>; details: unknown }> => {
		const started = Date.now();
		const bridge = getBridge();
		if (!bridge) {
			return reply(
				"lean-ctx MCP bridge is not connected, so ctx_bg cannot run. Use ctx_shell instead.",
				{ started, chars: 0 },
			);
		}
		const command = String(args.command ?? "").trim();
		if (!command) {
			return reply("command is required to start a background job.", { started, chars: 0 });
		}
		const env: Record<string, string> = {};
		const extra = args.env;
		if (extra !== undefined) {
			if (typeof extra !== "object" || extra === null || Array.isArray(extra)) {
				return reply("env must be an object of string to string.", { started, chars: 0 });
			}
			for (const [key, value] of Object.entries(extra as Record<string, unknown>)) {
				if (key.toUpperCase().startsWith("LEAN_CTX")) {
					// lean-ctx drops the whole run on a LEAN_CTX* key, so a start here
					// would report a job that never existed.
					return reply(
						`ctx_bg refused env key "${key}": lean-ctx refuses every LEAN_CTX* variable and runs nothing. Nothing ran; use another key name.`,
						{ started, chars: 0 },
					);
				}
				if (typeof value === "string") env[key] = value;
			}
		}
		// Job ids are content-addressed over command, cwd, timeout and env, so the
		// nonce is the only way to get a second job for an identical command.
		if (args.unique === true) env.CTXBG_NONCE = randomUUID();
		const payload: Record<string, unknown> = { command, run_in_background: true };
		if (typeof args.cwd === "string" && args.cwd) payload.cwd = args.cwd;
		if (typeof args.timeout_ms === "number") payload.timeout_ms = args.timeout_ms;
		if (Object.keys(env).length > 0) payload.env = env;

		let output: string;
		try {
			// No override keeps the bridge default of 120 s for a start; 0 disables it.
			output = textOf(await callBridge(bridge, payload, rpcTimeoutOf(args), signal));
		} catch (err) {
			return reply(`ctx_bg failed: ${messageOf(err)}`, { started, chars: 0 });
		}
		const chars = output.length;
		const id = STARTED.exec(output)?.[1];
		if (!id) {
			return reply(`lean-ctx did not return a job id:\n${clip(output)}`, { started, chars });
		}
		if (jobs.size >= MAX_TRACKED_JOBS) {
			// The job exists in lean-ctx either way; only the wake is skipped.
			return reply(
				`${output}\n[not tracked for a wake message: ${MAX_TRACKED_JOBS} jobs already tracked]`,
				{ started, chars, jobId: id },
				{ name: String(args.name ?? "").trim() || id, jobId: id, elapsedMs: 0 },
			);
		}
		const wallMs = wallMsOf(args);
		const job: Job = {
			id,
			name: String(args.name ?? "").trim().slice(0, 60),
			command,
			timer: setInterval(() => void poll(job), POLL_MS),
			pollMs: POLL_MS,
			slow: false,
			wallTimer: undefined,
			wallMs,
			wallRetried: false,
			note: "",
			wake: compileWake(typeof args.wake_on === "string" ? args.wake_on : undefined),
			wakes: 0,
			polls: 0,
			startedAt: Date.now(),
			lastLine: "",
			wokenLines: new Set<string>(),
			dryRun: undefined,
		};
		job.timer.unref?.();
		jobs.set(id, job);
		emitJobsActive("start", job);
		if (wallMs > 0) {
			const wallTimer = setTimeout(() => {
				job.wallTimer = undefined;
				void wallStop(job);
			}, wallMs);
			wallTimer.unref?.();
			job.wallTimer = wallTimer;
		}
		showJobs();
		return reply(
			`${output}\n[a follow-up message arrives when it finishes. Continue with other work instead of polling.]`,
			{ started, chars, jobId: id },
			jobHead(job),
		);
	};

	const inspect = async (
		action: "status" | "cancel",
		jobId: string,
		rpcTimeout: number | undefined,
		signal?: AbortSignal,
	): Promise<{ content: Array<{ type: "text"; text: string }>; details: unknown }> => {
		const started = Date.now();
		const bridge = getBridge();
		if (!bridge) {
			return reply(
				"lean-ctx MCP bridge is not connected, so ctx_bg cannot inspect a job.",
				{ started, chars: 0, jobId },
			);
		}
		let output: string;
		try {
			output = textOf(
				await callBridge(
					bridge,
					{ background_action: action, job_id: jobId },
					rpcTimeout ?? RPC_STATUS_TIMEOUT_MS,
					signal,
				),
			);
		} catch (err) {
			return reply(`ctx_bg ${action} failed: ${messageOf(err)}`, { started, chars: 0, jobId });
		}
		const job = jobs.get(jobId);
		if (job && !STILL_RUNNING.test(output)) stopTracking(job);
		return reply(clip(output), { started, chars: output.length, jobId }, job ? jobHead(job) : { jobId });
	};

	const forceKill = async (
		jobId: string,
		confirm: boolean,
	): Promise<{ content: Array<{ type: "text"; text: string }>; details: unknown }> => {
		const started = Date.now();
		const job = jobs.get(jobId);
		if (!job) {
			return reply(
				`ctx_bg force_kill has no record for job ${jobId}, so nothing was killed.`,
				{ started, chars: 0, jobId },
			);
		}
		const token = killToken(job.command);
		if (!token) {
			// A token from the interpreter or from `lean-ctx` would sweep every shell
			// or the bridge, so a command without a distinctive word gets no sweep.
			return reply(
				`ctx_bg force_kill cannot sweep for job ${job.name || job.id} ($ ${job.command}): `
					+ "the command holds no script path and no distinctive argument of 4 characters or more, "
					+ "so the only token left would match unrelated processes. Nothing was killed; "
					+ "run the job through a script path or pass a distinctive argument.",
				{ started, chars: 0, jobId },
				jobHead(job),
			);
		}
		// A confirm may reach only the pids an earlier dry run listed, so a missing,
		// empty, foreign or old list refuses before the table is read again.
		const record = job.dryRun;
		if (confirm) {
			if (!record || record.pids.length === 0 || record.token !== token) {
				return reply(
					`ctx_bg force_kill for job ${job.name || job.id} has no usable dry-run list, so nothing was killed. `
						+ "Run background_action=\"force_kill\" without confirm to list the matched processes, "
						+ "then repeat the call with confirm:true.",
					{ started, chars: 0, jobId },
				);
			}
			if (Date.now() - record.at > DRY_RUN_TTL_MS) {
				return reply(
					`ctx_bg force_kill for job ${job.name || job.id} refused: its dry-run list is older than `
						+ `${DRY_RUN_TTL_MS / 60000} minutes, so those pids may have been reused. Nothing was killed; `
						+ "run the dry run again and confirm the rows it lists.",
					{ started, chars: 0, jobId },
				);
			}
		}
		let output: string;
		try {
			output = await runPs();
		} catch (err) {
			return reply(
				`ctx_bg force_kill could not read the process table: ${messageOf(err)}`,
				{ started, chars: 0, jobId },
			);
		}
		const rows = parsePs(output);
		const jobSec = Math.floor((Date.now() - job.startedAt) / 1000);
		const { targets, stale, blocked, own } = pickTargets(
			rows,
			process.pid,
			token,
			jobSec + KILL_SLACK_SEC,
		);
		if (!own.verifiable) {
			// The table holds no row for this process, so neither our own group nor
			// the groups above us are known. Fail closed: list nothing, kill nothing.
			return reply(
				`ctx_bg force_kill could not verify the process table for job ${job.name || job.id}: `
					+ `it holds no row for pid ${process.pid}, so the process group of this session and of its parents is unknown `
					+ "and no row is safe to signal. Nothing was killed and no row was listed.",
				{ started, chars: output.length, jobId },
			);
		}
		const lines = [
			`force_kill job ${job.name || job.id} ($ ${job.command}) · token "${token}" · job age ${jobSec}s`,
		];
		// Rows the sweep refuses to signal are named in both branches, so a dry run
		// and its confirm read the same.
		const safetyLines = (): string[] => {
			const out: string[] = [];
			if (blocked.length > 0) {
				out.push(
					`${blocked.length} matched row(s) share the process group of a pi ancestor and were left alone: `
						+ `${blocked.map((row) => row.pid).join(", ")}.`,
				);
			}
			out.push(
				`${stale.length} matched row(s) ran longer than the job by more than ${KILL_SLACK_SEC}s and were left alone.`,
			);
			return out;
		};
		if (!confirm) {
			if (targets.length === 0) {
				lines.push("no process matched the job token, so nothing was killed.");
			} else {
				lines.push(...psTable(targets, own));
			}
			lines.push(...safetyLines());
			// The list this call shows is the only set a later confirm may reach.
			job.dryRun = { pids: targets.map((row) => row.pid), token, at: Date.now() };
			return reply(
				lines.join("\n"),
				{ started, chars: output.length, jobId },
				{
					jobId,
					pids: targets.map((row) => row.pid),
					listed: targets.length,
					stale: stale.length,
					dryRun: true,
				},
				`dry run: nothing was killed. To SIGKILL the ${targets.length} matched process(es), repeat this call with confirm:true`,
			);
		}
		// Same token, same exclusions, same age cut, and only pids the dry run
		// listed: a row that appeared since then never takes a signal.
		const listedPids = record?.pids ?? [];
		const wanted = new Set(listedPids);
		const killable = targets.filter((row) => wanted.has(row.pid));
		const unlisted = targets.filter((row) => !wanted.has(row.pid));
		const droppedPids = listedPids.filter((pid) => !killable.some((row) => row.pid === pid));
		const reasonFor = (pid: number): string => {
			if (stale.some((row) => row.pid === pid)) return "now stale";
			if (blocked.some((row) => row.pid === pid)) return "pi ancestor group";
			if (rows.some((row) => row.pid === pid)) return "no longer matches";
			return "gone";
		};
		let killed = 0;
		for (const row of killable) {
			try {
				process.kill(killSignal(row, own).signal, "SIGKILL");
				killed += 1;
			} catch {
				// The row died between ps and the kill; that is not a kill failure.
			}
		}
		job.dryRun = undefined;
		if (killable.length > 0) {
			lines.push(...psTable(killable, own));
		} else {
			lines.push("no listed process still matched, so nothing was killed.");
		}
		lines.push(...safetyLines());
		lines.push(
			`the dry run listed ${listedPids.length} pid(s); killed ${killed} of ${killable.length} still matched.`,
		);
		if (unlisted.length > 0) {
			lines.push(
				`${unlisted.length} matched row(s) appeared after the dry run and were left alone: `
					+ `${unlisted.map((row) => row.pid).join(", ")}.`,
			);
		}
		if (droppedPids.length > 0) {
			lines.push(
				`dropped ${droppedPids.length} listed pid(s): `
					+ `${droppedPids.map((pid) => `${pid} (${reasonFor(pid)})`).join(", ")}.`,
			);
		} else {
			lines.push("dropped no listed pid.");
		}
		return reply(
			lines.join("\n"),
			{ started, chars: output.length, jobId },
			{
				...jobHead(job),
				pids: killable.map((row) => row.pid),
				killed,
				dropped: droppedPids,
				unlisted: unlisted.map((row) => row.pid),
				stale: stale.length,
			},
		);
	};

	registerTool({
		name: "ctx_bg",
		label: "ctx_bg",
		description:
			"Run a shell command as a lean-ctx background job. Returns a job id at once and wakes you with the "
			+ "result when it finishes, so long builds, test suites, downloads and training runs do not block the turn. "
			+ "The command keeps the lean-ctx shell allowlist and compressed output. "
			+ "Use background_action \"status\" or \"cancel\" with a job_id only when you must inspect or stop a job early. "
			+ "background_action \"force_kill\" lists the processes a tracked job left behind and kills nothing "
			+ "until you repeat the call with confirm:true, which SIGKILLs only the pids that dry run listed.",
		promptSnippet: "Run a long shell command in the background and get woken when it finishes",
		promptGuidelines: [
			"Always pass name: a short human label for the command, so the user can tell the job apart in the jobs widget.",
			"Use ctx_bg instead of ctx_shell for commands that may take longer than about 30 seconds: builds, full test suites, installs, large downloads, training runs.",
			"force_kill is a dry run first: read the matched rows, then repeat it with confirm:true to kill them. The confirm kills only the pids that dry run listed, and its list expires after 10 minutes.",
			"After ctx_bg, continue with other work or end the turn. A follow-up message with the result arrives by itself, so never poll or sleep to wait for a job.",
			"Jobs stop when Pi exits, because they live in the lean-ctx process attached to this session.",
		],
		parameters: TEXT,
		// The reply text carries the body, then the rpc footer, then an optional
		// hint. The band, the bar and the glyph follow details.outcome, so a failed
		// job is told apart from a finished one at a glance.
		renderResult(
			result: { content?: Array<{ text?: string }>; details?: unknown },
			{ expanded }: { expanded: boolean; isPartial?: boolean },
			theme: any,
		) {
			const text = textOf(result as { content: Array<{ text?: string }> });
			const details = (result.details ?? {}) as {
				name?: unknown; jobId?: unknown; elapsedMs?: unknown; exitCode?: unknown;
			};
			const outcome = outcomeOf(result.details, text);
			const all = text.split("\n");
			const at = all.findIndex((line) => RPC_LINE.test(line));
			const footer = at < 0 ? "" : all[at] ?? "";
			const body = at < 0 ? all : all.filter((_line, index) => index !== at);
			while (body.length > 0 && body[body.length - 1] === "") body.pop();
			const cap = expanded ? RESULT_BODY_LINES_EXPANDED : RESULT_BODY_LINES;
			const shown = body.slice(0, cap);
			const name = String(details.name || details.jobId || "ctx_bg");
			// Box paints its padding rows through bgFn, so padY 1 gives the band one
			// blank row above and below, like a regular tool block.
			const box = new Box(1, 1, (line: string) => background(theme, line, BAND[outcome]));
			box.addChild(new Text(
				barred(
					theme,
					outcome,
					headLine(theme, {
						name,
						elapsedMs: typeof details.elapsedMs === "number" ? details.elapsedMs : 0,
						outcome,
						exitCode: typeof details.exitCode === "number" ? details.exitCode : undefined,
						jobId: String(details.jobId ?? "-"),
					}),
				),
				0,
				0,
			));
			for (const line of shown) {
				box.addChild(new Text(barred(theme, outcome, theme.fg("text", line)), 0, 0));
			}
			if (body.length > shown.length) {
				box.addChild(
					new Text(barred(theme, outcome, theme.fg("dim", `… ${body.length - shown.length} more lines`)), 0, 0),
				);
			}
			if (footer) box.addChild(new Text(barred(theme, outcome, theme.fg("dim", footer)), 0, 0));
			return box;
		},
		async execute(
			_toolCallId: string,
			params: Record<string, unknown>,
			signal: AbortSignal,
			_onUpdate?: unknown,
			ctx?: ExtensionContext,
		) {
			adoptContext(ctx);
			if (params.background_action === "status" || params.background_action === "cancel") {
				const jobId = String(params.job_id ?? "");
				if (!jobId) return missingJobId();
				return inspect(params.background_action, jobId, rpcTimeoutOf(params), signal);
			}
			if (params.background_action === "force_kill") {
				const jobId = String(params.job_id ?? "");
				if (!jobId) return missingJobId();
				return forceKill(jobId, params.confirm === true);
			}
			return start(params, signal);
		},
	});

	pi.registerMessageRenderer("lean-ctx-background", (message: { content?: unknown; details?: unknown }, { expanded }: { expanded: boolean }, theme: any) => {
		const body = typeof message.content === "string" ? message.content : "";
		const [head, ...rest] = body.split("\n");
		const outcome = outcomeOf(message.details, body);
		const details = (message.details ?? {}) as {
			name?: unknown; jobId?: unknown; elapsedMs?: unknown; exitCode?: unknown;
		};
		// An older message may carry no job id, so the id in its prose stands in.
		const jobId = String(details.jobId ?? /\bjob (\S+)/.exec(head ?? "")?.[1] ?? "-");
		const tail = expanded ? rest : rest.slice(0, MESSAGE_BODY_LINES);
		const lines = [
			headLine(theme, {
				name: String(details.name || jobId),
				elapsedMs: typeof details.elapsedMs === "number" ? details.elapsedMs : 0,
				outcome,
				exitCode: typeof details.exitCode === "number" ? details.exitCode : undefined,
				jobId,
			}),
			...tail.map((line: string) => theme.fg("text", ` ${line}`)),
		];
		if (!expanded && rest.length > tail.length) lines.push(theme.fg("dim", ` +${rest.length - tail.length} lines`));
		const box = new Box(1, 1, (line: string) => background(theme, line, BAND[outcome]));
		for (const line of lines) box.addChild(new Text(barred(theme, outcome, line), 0, 0));
		return box;
	});

	pi.on("session_start", async (_event, ctx) => {
		stopped = false;
		// A session boundary opens a fresh widget slot, so this session registers
		// once and keeps that component for its whole life.
		widgetState.registered = false;
		ui = ctx.mode === "tui" ? ctx.ui : undefined;
		showJobs();
	});

	pi.on("session_shutdown", async () => {
		stopped = true;
		stopTicker();
		for (const job of jobs.values()) {
			clearInterval(job.timer);
			if (job.wallTimer) clearTimeout(job.wallTimer);
		}
		jobs.clear();
		ui?.setWidget(WIDGET_KEY, undefined);
		// The next session starts with a fresh slot, so it registers once again.
		resetWidget();
		// A cleared handle blocks every later paint, so a retry timer that fires
		// after shutdown stays silent.
		ui = undefined;
	});
}
