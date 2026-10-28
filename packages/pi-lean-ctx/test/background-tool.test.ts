import { execFile } from "node:child_process";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
	boundMessageBody,
	classifyOutcome,
	ICON,
	killSignal,
	ownGroups,
	parseElapsed,
	psTable,
	registerBackgroundTool,
} from "../extensions/background-tool.js";

// force_kill reads the process table through execFile; the fake table below
// keeps the filter test off the real process list.
vi.mock("node:child_process", () => ({ execFile: vi.fn() }));

type SentMessage = { customType: string; content: string; details: Record<string, unknown> };
type SentOptions = { triggerTurn?: boolean; deliverAs?: string };

const RUNNING = (id: string, output = "step 1 of 10") => `[background:${id} running]\n${output}`;
const DONE = (id: string, code = 0) => `[background:${id} completed exit=${code}]\nall tests passed`;
const STARTED = (id: string) =>
	`[background:${id} started — use ctx_shell(background_action="status", job_id="${id}") to poll or background_action="cancel" to stop it]`;

type Execute = (
	toolCallId: string,
	params: Record<string, unknown>,
	signal?: AbortSignal,
	onUpdate?: unknown,
	ctx?: unknown,
) => Promise<{ content: Array<{ text: string }>; details?: unknown }>;

type Component = { render(width: number): string[] };
type RenderResult = (
	result: unknown,
	options: { expanded: boolean; isPartial?: boolean },
	theme: unknown,
) => Component;
type Renderer = (message: unknown, options: { expanded: boolean }, theme: unknown) => Component;

interface ToolDef {
	execute: Execute;
	renderResult: RenderResult;
}

interface Harness {
	sent: Array<{ message: SentMessage; options: SentOptions }>;
	shutdown: Array<() => Promise<void>>;
	callTool: ReturnType<typeof vi.fn>;
	tool: () => ToolDef;
	rendererType: () => string;
	renderer: () => Renderer;
	execute: (
		params: Record<string, unknown>,
		ctx?: unknown,
		signal?: AbortSignal,
	) => Promise<{ content: Array<{ text: string }>; details?: unknown }>;
	runShutdown: () => Promise<void>;
	runStart: (ctx: unknown) => Promise<void>;
}

// A theme that records the band token of every painted line and every fg token
// instead of adding escape codes, so a test can name the band and the colour a
// block wears.
function bandTheme(): {
	theme: Record<string, unknown>;
	tokens: string[];
	bands: Array<{ token: string; text: string }>;
	fg: Array<{ token: string; text: string }>;
} {
	const tokens: string[] = [];
	const bands: Array<{ token: string; text: string }> = [];
	const fg: Array<{ token: string; text: string }> = [];
	return {
		tokens,
		bands,
		fg,
		theme: {
			fg: (token: string, text: string) => {
				fg.push({ token, text });
				return text;
			},
			bold: (text: string) => text,
			bg: (token: string, text: string) => {
				tokens.push(token);
				bands.push({ token, text });
				return text;
			},
		},
	};
}

const drawn = (component: Component, width = 80): string => component.render(width).join("\n");

function harness(
	bridge: { callTool: ReturnType<typeof vi.fn> } | undefined,
): Harness {
	const sent: Harness["sent"] = [];
	const shutdown: Harness["shutdown"] = [];
	const start: Array<(event: unknown, ctx: unknown) => Promise<void>> = [];
	let registered: ToolDef | undefined;
	let rendererType = "";
	let renderer: Renderer | undefined;

	registerBackgroundTool({
		pi: {
			sendMessage: (message: SentMessage, options: SentOptions) => {
				sent.push({ message, options });
			},
			registerMessageRenderer: (type: string, fn: unknown) => {
				rendererType = type;
				renderer = fn as Renderer;
			},
			on: (event: string, handler: (event: unknown, ctx: unknown) => Promise<void>) => {
				if (event === "session_shutdown") shutdown.push(handler as () => Promise<void>);
				if (event === "session_start") start.push(handler);
			},
		} as never,
		registerTool: ((def: unknown) => {
			registered = def as ToolDef;
		}) as never,
		getBridge: () => bridge as never,
	});

	return {
		sent,
		shutdown,
		callTool: bridge?.callTool ?? vi.fn(),
		tool: () => registered!,
		rendererType: () => rendererType,
		renderer: () => renderer!,
		execute: (params, ctx, signal) => registered!.execute("call-1", params, signal as AbortSignal, undefined, ctx as never),
		runShutdown: async () => {
			for (const handler of shutdown) await handler();
		},
		runStart: async (ctx: unknown) => {
			for (const handler of start) await handler({ type: "session_start", reason: "startup" }, ctx);
		},
	};
}

const textOf = (result: { content: Array<{ text: string }> }) => result.content[0].text;

const cancelCalls = (callTool: ReturnType<typeof vi.fn>) =>
	callTool.mock.calls.filter(([, args]) => (args as { background_action?: string }).background_action === "cancel");

// Every force_kill test reads the same fake process table instead of the real one.
const psReturns = (rows: string[]) =>
	vi.mocked(execFile).mockImplementation(((
		_file: string,
		_args: string[],
		_options: unknown,
		callback: (error: Error | null, stdout: string, stderr: string) => void,
	) => callback(null, rows.join("\n"), "")) as never);

// process.getpgrp is not part of the Node API, so a test states our own group by
// putting a row for this pid in the fake table.
const selfRow = (pgid: number) =>
	`  ${process.pid}  ${process.ppid}  ${pgid}  ${pgid}       1 node /repo/extensions/background-tool.ts`;

// A confirm re-reads the table, so the dry run and the confirm can be handed
// different process lists.
const psReturnsSequence = (tables: string[][]) => {
	let call = 0;
	vi.mocked(execFile).mockImplementation(((
		_file: string,
		_args: string[],
		_options: unknown,
		callback: (error: Error | null, stdout: string, stderr: string) => void,
	) => {
		const rows = tables[Math.min(call, tables.length - 1)] ?? [];
		call += 1;
		callback(null, rows.join("\n"), "");
	}) as never);
};

describe("ctx_bg", () => {
	beforeEach(() => {
		vi.useFakeTimers();
		// The ps mock is module-wide, so each test starts with an empty call log.
		vi.mocked(execFile).mockReset();
	});

	afterEach(() => {
		vi.useRealTimers();
		vi.restoreAllMocks();
	});

	it("starts a job in background mode and reports the job id", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_a1") }] });
		const { execute } = harness({ callTool });

		const result = await execute({ command: "cargo test" });

		expect(callTool).toHaveBeenCalledWith("ctx_shell", { command: "cargo test", run_in_background: true });
		expect(textOf(result)).toContain("shell_a1");
		expect(textOf(result)).toContain("follow-up message");
		expect(textOf(result)).toMatch(/rpc_ms=\d+ payload_chars=\d+ job_id=shell_a1$/);
	});

	it("passes cwd and timeout_ms through when given", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_a2") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "make", cwd: "/tmp/x", timeout_ms: 600_000 });

		expect(callTool).toHaveBeenCalledWith("ctx_shell", {
			command: "make",
			run_in_background: true,
			cwd: "/tmp/x",
			timeout_ms: 600_000,
		});
	});

	it("wakes with a follow-up message once the job reaches a terminal state", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_b1") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: RUNNING("shell_b1") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: DONE("shell_b1") }] });
		const { execute, sent } = harness({ callTool });

		await execute({ command: "go test ./..." });
		expect(sent).toHaveLength(0);

		await vi.advanceTimersByTimeAsync(2000);
		expect(sent).toHaveLength(0);

		await vi.advanceTimersByTimeAsync(2000);
		expect(sent).toHaveLength(1);
		expect(sent[0].options).toEqual({ triggerTurn: true, deliverAs: "followUp" });
		expect(sent[0].message.customType).toBe("lean-ctx-background");
		expect(sent[0].message.content).toContain("finished");
		expect(sent[0].message.content).toContain("all tests passed");

		// The poller stops after the terminal poll, so no second wake appears.
		await vi.advanceTimersByTimeAsync(6000);
		expect(sent).toHaveLength(1);
		expect(callTool).toHaveBeenCalledTimes(3);
	});

	it("bounds a chatty body at the terminal wake", async () => {
		const chatty = Array.from({ length: 200 }, (_v, i) => `compiling unit ${i + 1}`);
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_b2") }] })
			.mockResolvedValueOnce({
				content: [{ type: "text", text: `[background:shell_b2 completed exit=0]\n${chatty.join("\n")}` }],
			});
		const { execute, sent } = harness({ callTool });

		await execute({ command: "make build" });
		await vi.advanceTimersByTimeAsync(2000);
		const content = sent[0].message.content;

		expect(content).toContain("$ make build");
		expect(content).toContain("… 195 lines hidden");
		expect(content).not.toContain("compiling unit 50");
		expect(content.length).toBeLessThan(1500);
	});

	it("bounds a chatty body at the wake-pattern site", async () => {
		const chatty = Array.from({ length: 200 }, (_v, i) => `compiling unit ${i + 1}`);
		chatty[100] = "listening on :3000";
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_b3") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: RUNNING("shell_b3", chatty.join("\n")) }] });
		const { execute, sent } = harness({ callTool });

		await execute({ command: "npm run dev", wake_on: "listening on" });
		await vi.advanceTimersByTimeAsync(2000);
		const content = sent[0].message.content;

		expect(String(sent[0].message.details.kind)).toBe("wake");
		expect(content).toContain("$ npm run dev");
		expect(content).toContain("… 195 lines hidden");
		expect(content).not.toContain("compiling unit 50");
		expect(content.length).toBeLessThan(1500);
	});

	it("keeps tracking through a transient bridge failure", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_c1") }] })
			.mockRejectedValueOnce(new Error("bridge closed"))
			.mockResolvedValueOnce({ content: [{ type: "text", text: DONE("shell_c1") }] });
		const { execute, sent } = harness({ callTool });

		await execute({ command: "npm run build" });
		await vi.advanceTimersByTimeAsync(2000);
		expect(sent).toHaveLength(0);

		await vi.advanceTimersByTimeAsync(2000);
		expect(sent).toHaveLength(1);
		expect(sent[0].message.content).toContain("finished");
	});

	it("wakes once for a matching line that a later poll re-reports", async () => {
		const callTool = vi.fn();
		callTool.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_d1") }] });
		for (let i = 0; i < 5; i += 1) {
			callTool.mockResolvedValueOnce({ content: [{ type: "text", text: RUNNING("shell_d1", "listening on :3000") }] });
		}
		const { execute, sent } = harness({ callTool });

		await execute({ command: "npm run dev", wake_on: "listening on" });
		await vi.advanceTimersByTimeAsync(2000 * 5);

		expect(sent).toHaveLength(1);
	});

	it("wakes once per distinct matching line, up to the cap", async () => {
		const callTool = vi.fn();
		callTool.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_d2") }] });
		for (let i = 0; i < 12; i += 1) {
			callTool.mockResolvedValueOnce({ content: [{ type: "text", text: RUNNING("shell_d2", `epoch ${i + 1}/10 loss 0.4${i}`) }] });
		}
		const { execute, sent } = harness({ callTool });

		await execute({ command: "python3 train.py", wake_on: "epoch \\d+" });
		await vi.advanceTimersByTimeAsync(2000 * 12);

		expect(sent).toHaveLength(10);
		expect(sent.every((entry) => String(entry.message.details.kind) === "wake")).toBe(true);
	});

	it("adopts the tool-call context so the jobs widget renders", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_h1") }] });
		const { execute } = harness({ callTool });
		const setWidget = vi.fn();

		await execute({ command: "sleep 60" }, { mode: "tui", ui: { setWidget } });

		expect(setWidget).toHaveBeenCalledWith("lean-ctx-jobs", expect.any(Function), { placement: "belowEditor" });
	});

	it("accepts Python's (?i) prefix in wake_on", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_e1") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: RUNNING("shell_e1", "Epoch 3/10 loss 0.41") }] });
		const { execute, sent } = harness({ callTool });

		await execute({ command: "python3 train.py", wake_on: "(?i)epoch 3" });
		await vi.advanceTimersByTimeAsync(2000);

		expect(sent).toHaveLength(1);
	});

	it("forwards status and cancel to the bridge", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_f1") }] });
		const { execute } = harness({ callTool });

		await execute({ background_action: "status", job_id: "shell_f1" });
		expect(callTool).toHaveBeenLastCalledWith(
			"ctx_shell",
			{ background_action: "status", job_id: "shell_f1" },
			undefined,
			15000,
		);

		await execute({ background_action: "cancel", job_id: "shell_f1" });
		expect(callTool).toHaveBeenLastCalledWith(
			"ctx_shell",
			{ background_action: "cancel", job_id: "shell_f1" },
			undefined,
			15000,
		);
	});

	it("uses rpc_timeout_ms for a status call when given", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_f2") }] });
		const { execute } = harness({ callTool });

		await execute({ background_action: "status", job_id: "shell_f2", rpc_timeout_ms: 30000 });

		expect(callTool).toHaveBeenLastCalledWith(
			"ctx_shell",
			{ background_action: "status", job_id: "shell_f2" },
			undefined,
			30000,
		);
	});

	it("passes the tool call abort signal to the bridge", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_f3") }] });
		const { execute } = harness({ callTool });
		const controller = new AbortController();

		await execute({ background_action: "status", job_id: "shell_f3" }, undefined, controller.signal);

		expect(callTool).toHaveBeenLastCalledWith(
			"ctx_shell",
			{ background_action: "status", job_id: "shell_f3" },
			controller.signal,
			15000,
		);
	});

	it("passes rpc_timeout_ms 0 so the bridge disables its bound", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_f4") }] });
		const { execute } = harness({ callTool });

		await execute({ background_action: "status", job_id: "shell_f4", rpc_timeout_ms: 0 });

		expect(callTool).toHaveBeenLastCalledWith(
			"ctx_shell",
			{ background_action: "status", job_id: "shell_f4" },
			undefined,
			0,
		);
	});

	it("stops tracking a job once a status poll reports a terminal state", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_g1") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: DONE("shell_g1") }] });
		const { execute, sent } = harness({ callTool });

		await execute({ command: "sleep 1" });
		// A status poll beats the interval and settles the job, so no later tick wakes.
		await execute({ background_action: "status", job_id: "shell_g1" });
		await vi.advanceTimersByTimeAsync(6000);

		expect(sent).toHaveLength(0);
	});

	it("stops tracking a job once the wall cancel succeeds", async () => {
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => ({
			content: [{
				type: "text",
				text: args.background_action === "status" ? RUNNING("shell_q1") : STARTED("shell_q1"),
			}],
		}));
		const { execute } = harness({ callTool });

		await execute({ command: "yes", wall_timeout_ms: 2000 });
		await vi.advanceTimersByTimeAsync(2000);
		expect(cancelCalls(callTool)).toHaveLength(1);

		// The job left the table with the successful cancel, so force_kill has no
		// record to sweep for and a later poll cannot wake.
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_q1" }));
		expect(body).toContain("no record");
	});

	it("re-arms the wall cancel once after a failed cancel", async () => {
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => {
			if (args.background_action === "cancel") throw new Error("bridge closed");
			return {
				content: [{
					type: "text",
					text: args.background_action === "status" ? RUNNING("shell_q2") : STARTED("shell_q2"),
				}],
			};
		});
		const { execute } = harness({ callTool });

		await execute({ command: "yes", wall_timeout_ms: 1000 });
		await vi.advanceTimersByTimeAsync(1000);
		expect(cancelCalls(callTool)).toHaveLength(1);

		await vi.advanceTimersByTimeAsync(30000);
		expect(cancelCalls(callTool)).toHaveLength(2);

		// One retry only: a second failure leaves the job to the poller.
		await vi.advanceTimersByTimeAsync(30000);
		expect(cancelCalls(callTool)).toHaveLength(2);
	});

	it("rejects a missing command, a missing job_id and a missing bridge", async () => {
		const callTool = vi.fn();
		const withBridge = harness({ callTool });

		expect(textOf(await withBridge.execute({ command: "  " }))).toContain("command is required");
		expect(textOf(await withBridge.execute({ background_action: "status" }))).toContain("job_id is required");

		const withoutBridge = harness(undefined);
		expect(textOf(await withoutBridge.execute({ command: "ls" }))).toContain("MCP bridge is not connected");
	});

	it("reports a reply without a job id instead of pretending to track it", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: "unexpected reply" }] });
		const { execute, sent } = harness({ callTool });

		const result = await execute({ command: "ls" });
		await vi.advanceTimersByTimeAsync(4000);

		expect(textOf(result)).toContain("did not return a job id");
		expect(sent).toHaveLength(0);
	});

	it("tracks at most 8 jobs and still returns the job id", async () => {
		let next = 0;
		const callTool = vi.fn(async () => {
			next += 1;
			return { content: [{ type: "text", text: STARTED(`shell_h${next}`) }] };
		});
		const { execute, sent } = harness({ callTool });

		for (let i = 0; i < 9; i += 1) await execute({ command: `job ${i}` });
		const ninth = await execute({ command: "job nine" });

		expect(textOf(ninth)).toContain("shell_h10");
		expect(textOf(ninth)).toContain("not tracked");

		// Only the eight tracked jobs keep polling.
		callTool.mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_h1") }] });
		await vi.advanceTimersByTimeAsync(2000);
		// Ten starts, but only the first eight are tracked and polled.
		expect(callTool).toHaveBeenCalledTimes(10 + 8);
		expect(sent).toHaveLength(0);
	});

	it("clears every poller on session shutdown", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_i1") }] })
			.mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_i1") }] });
		const { execute, runShutdown, sent } = harness({ callTool });

		await execute({ command: "watch ls" });
		await runShutdown();
		await vi.advanceTimersByTimeAsync(10_000);

		expect(callTool).toHaveBeenCalledTimes(1);
		expect(sent).toHaveLength(0);
	});

	it("renders one jobs widget line per running job, then clears the widget", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_w1") }] })
			.mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_w1", "compiling 42 of 120") }] });
		const { execute, runShutdown, runStart } = harness({ callTool });
		const setWidget = vi.fn();
		await runStart({ mode: "tui", ui: { setWidget } });

		await execute({ command: "cargo test" });
		expect(setWidget).toHaveBeenCalledWith("lean-ctx-jobs", expect.any(Function), { placement: "belowEditor" });

		await vi.advanceTimersByTimeAsync(2000);
		const factory = setWidget.mock.calls.at(-1)?.[1] as (
			tui: { requestRender: () => void },
			theme: { fg: (color: string, text: string) => string },
		) => { render(width: number): string[]; dispose?: () => void };
		const component = factory({ requestRender: () => {} }, { fg: (_color, text) => text });
		const lines = component.render(200);

		expect(lines).toHaveLength(1);
		expect(lines[0]).toContain("2s  shell_w1  compiling 42 of 120");
		expect(lines[0]).toContain("compiling 42 of 120");
		component.dispose?.();

		await runShutdown();
		expect(setWidget).toHaveBeenLastCalledWith("lean-ctx-jobs", undefined);
	});

	it("renders the agent-supplied job name instead of the raw job id", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_n1") }] })
			.mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_n1", "compiling core") }] });
		const { execute } = harness({ callTool });
		const setWidget = vi.fn();

		await execute({ command: "nix build .#piCompiled", name: "compiled pi build" }, { mode: "tui", ui: { setWidget } });
		await vi.advanceTimersByTimeAsync(2000);

		const factory = setWidget.mock.calls.at(-1)?.[1] as (
			tui: { requestRender: () => void },
			theme: { fg: (color: string, text: string) => string },
		) => { render(width: number): string[] };
		const lines = factory({ requestRender: () => {} }, { fg: (_color, text) => text }).render(120);

		expect(lines[0]).toContain("compiled pi build");
		expect(lines[0]).toContain("compiling core");
		expect(lines[0]).not.toContain("shell_n1");
	});

	it("puts elapsed first and holds its column for a short and a long name", async () => {
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => {
			const status = args.background_action === "status";
			const long = args.command === "make b" || args.job_id === "shell_l1";
			const id = long ? "shell_l1" : "shell_s1";
			return {
				content: [{ type: "text", text: status ? RUNNING(id, `output of ${id}`) : STARTED(id) }],
			};
		});
		const { execute } = harness({ callTool });
		const setWidget = vi.fn();

		await execute({ command: "make a", name: "build" }, { mode: "tui", ui: { setWidget } });
		await execute(
			{ command: "make b", name: "integration test suite for the whole repository" },
			{ mode: "tui", ui: { setWidget } },
		);
		await vi.advanceTimersByTimeAsync(2000);

		const factory = setWidget.mock.calls.at(-1)?.[1] as (
			tui: { requestRender: () => void },
			theme: { fg: (color: string, text: string) => string },
		) => { render(width: number): string[] };
		const lines = factory({ requestRender: () => {} }, { fg: (_color, text) => text }).render(80);

		expect(lines).toHaveLength(2);
		// The clock glyph opens the row and its fixed column holds the name start,
		// so the long label truncates instead of shifting the elapsed field.
		expect(lines[0].slice(0, 9)).toBe(` ${ICON.clock}   2s  `);
		expect(lines[1].slice(0, 9)).toBe(` ${ICON.clock}   2s  `);
		expect(lines[0]).toContain("  build  output of shell_s1");
		expect(lines[1]).toContain("  integration");
	});

	it("refuses a LEAN_CTX env key without calling the bridge", async () => {
		const callTool = vi.fn();
		const { execute } = harness({ callTool });

		const result = await execute({ command: "make", env: { lean_ctx_token: "secret" } });

		expect(callTool).not.toHaveBeenCalled();
		expect(textOf(result)).toContain("lean_ctx_token");
		expect(textOf(result)).toContain("Nothing ran");
	});

	it("merges env into the start payload", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_o1") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "make", env: { RUST_LOG: "debug" } });

		const payload = callTool.mock.calls[0]?.[1] as { env: Record<string, string> };
		expect(payload.env).toEqual({ RUST_LOG: "debug" });
	});

	it("adds a nonce env pair when unique is set", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_o2") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "npm ci", unique: true });

		const payload = callTool.mock.calls[0]?.[1] as { env: Record<string, string> };
		expect(payload.env.CTXBG_NONCE).toMatch(/^[0-9a-f-]{36}$/);
		expect(payload.env.CTXBG_NONCE.startsWith("LEAN_CTX")).toBe(false);
	});

	it("cancels once when the wall limit fires", async () => {
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => ({
			content: [{
				type: "text",
				text: args.background_action === "status" ? RUNNING("shell_j1") : STARTED("shell_j1"),
			}],
		}));
		const { execute, sent } = harness({ callTool });

		await execute({ command: "yes", wall_timeout_ms: 5000 });
		await vi.advanceTimersByTimeAsync(4000);
		expect(cancelCalls(callTool)).toHaveLength(0);

		await vi.advanceTimersByTimeAsync(1000);
		expect(cancelCalls(callTool)).toHaveLength(1);
		expect(sent).toHaveLength(1);
		expect(sent[0].message.content).toContain("wall limit");

		await vi.advanceTimersByTimeAsync(20000);
		expect(cancelCalls(callTool)).toHaveLength(1);
	});

	it("polls every 30 s once the status text passes the guard", async () => {
		const big = `${RUNNING("shell_k1")}\n${"x".repeat(100_100)}`;
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_k1") }] })
			.mockResolvedValue({ content: [{ type: "text", text: big }] });
		const { execute } = harness({ callTool });

		await execute({ command: "yes" });
		await vi.advanceTimersByTimeAsync(2000);
		expect(callTool).toHaveBeenCalledTimes(2);

		await vi.advanceTimersByTimeAsync(28000);
		expect(callTool).toHaveBeenCalledTimes(2);

		await vi.advanceTimersByTimeAsync(2000);
		expect(callTool).toHaveBeenCalledTimes(3);
	});

	it("force_kill keeps the escaped row and drops the self, parent, child, bridge and scratch rows", async () => {
		const rows = [
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			`  ${process.pid}  ${process.ppid}  4240  4240       1 node /repo/extensions/background-tool.ts`,
			`  ${process.ppid}     1  4240  4240     600 sh /repo/scripts/test-all.sh --all`,
			`  4100  ${process.pid}  4100  4100       2 /repo/bin/lean-ctx server`,
			"  1000     1  1000  1000       1 ps -eo pid,ppid,pgid,sid,etimes,args",
			"  4242     1  4242  4242       3 sh /repo/scripts/test-all.sh --all",
			"  5555     1  5555  5555       3 cargo test --all",
			"  6666     1  6666  6666       3 sh",
			"  7777     1  7777  7777     900 /repo/scripts/test-all.sh",
		];
		psReturns(rows);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_m1") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /repo/scripts/test-all.sh --all" });
		await execute({ background_action: "force_kill", job_id: "shell_m1" });
		const result = await execute({ background_action: "force_kill", job_id: "shell_m1", confirm: true });
		const body = textOf(result);

		expect(kill).toHaveBeenCalledTimes(1);
		// Row 4242 leads its own group, so the whole group takes the signal.
		expect(kill).toHaveBeenCalledWith(-4242, "SIGKILL");
		expect(body).toContain('token "/repo/scripts/test-all.sh"');
		expect(body).toContain("4242");
		expect(body).toContain("killed 1 of 1");
		expect(body).not.toContain("background-tool.ts");
		expect(body).not.toContain("5555");
		expect(body).not.toContain("6666");
		expect(body).not.toContain("4100");
		expect(body).toContain("1 matched row(s)");
	});

	it("force_kill matches the row that holds the script path and skips a bare sh row", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			"  4300     1  4300  4300       2 sh /tmp/x/a4.sh",
			"  6600     1  6600  6600       2 sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p1") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/a4.sh" });
		await execute({ background_action: "force_kill", job_id: "shell_p1" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p1", confirm: true }));

		expect(body).toContain('token "/tmp/x/a4.sh"');
		expect(kill).toHaveBeenCalledTimes(1);
		expect(kill).toHaveBeenCalledWith(-4300, "SIGKILL");
		expect(body).toContain("4300");
		expect(body).not.toContain("6600");
	});

	it("force_kill refuses a command with no script path or distinctive argument", async () => {
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_p2") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_p3") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "lean-ctx mcp serve" });
		const first = textOf(await execute({ background_action: "force_kill", job_id: "shell_p2", confirm: true }));

		await execute({ command: "sleep 400" });
		const second = textOf(await execute({ background_action: "force_kill", job_id: "shell_p3", confirm: true }));

		expect(first).toContain("no script path");
		expect(first).toContain("lean-ctx mcp serve");
		expect(second).toContain("distinctive argument");
		expect(second).toContain("sleep 400");
		expect(vi.mocked(execFile)).not.toHaveBeenCalled();
		expect(kill).not.toHaveBeenCalled();
	});

	it("force_kill without confirm lists the table and kills nothing", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			"  4500  4400  4400  4400       2 sh /tmp/x/a5.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p4") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/a5.sh" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p4" }));

		expect(kill).not.toHaveBeenCalled();
		expect(body).toContain("4500");
		expect(body).toContain("dry run");
		expect(body.split("\n").at(-1)).toContain("confirm:true");
	});

	it("force_kill with confirm sends SIGKILL to the matched pid", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			"  4500  4400  4400  4400       2 sh /tmp/x/a5.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p5") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/a5.sh" });
		await execute({ background_action: "force_kill", job_id: "shell_p5" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p5", confirm: true }));

		// Row 4500 does not lead its group, so only that pid takes the signal.
		expect(kill).toHaveBeenCalledWith(4500, "SIGKILL");
		expect(body).toContain("killed 1 of 1");
	});

	it("force_kill never matches a direct child of pi", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			`  4600  ${process.pid}  4600  4600       2 sh /tmp/x/a6.sh`,
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p6") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/a6.sh" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p6" }));
		// An empty dry-run list carries nothing a confirm could kill.
		const confirmed = textOf(await execute({ background_action: "force_kill", job_id: "shell_p6", confirm: true }));

		expect(kill).not.toHaveBeenCalled();
		expect(body).toContain("no process matched");
		expect(confirmed).toContain("no usable dry-run list");
	});

	it("force_kill sends the positive pid for a row in our own process group and marks group no", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4800),
			"  4805  4400  4800  4800       2 sh /tmp/x/g1.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p7") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/g1.sh" });
		await execute({ background_action: "force_kill", job_id: "shell_p7" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p7", confirm: true }));

		// A group signal on our own group would reach pi, so the row keeps its pid.
		expect(kill).toHaveBeenCalledTimes(1);
		expect(kill).toHaveBeenCalledWith(4805, "SIGKILL");
		expect(body).toContain("group");
		const row = body.split("\n").find((line) => /^\s+4805\s/.test(line)) ?? "";
		expect(row).toMatch(/\sno\s+sh \/tmp\/x\/g1\.sh$/);
	});

	it("keeps the group form for a leader outside our own process group", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(9999),
			"  4810  4400  4810  4810       2 sh /tmp/x/g2.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_p8") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/g2.sh" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_p8" }));

		const row = body.split("\n").find((line) => /^\s+4810\s/.test(line)) ?? "";
		expect(row).toMatch(/\syes\s+sh \/tmp\/x\/g2\.sh$/);
	});

	it("drops every leading interpreter word so the token is the script path", async () => {
		psReturns([selfRow(4240)]);
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_s1") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_s2") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "/bin/bash script.sh" });
		const first = textOf(await execute({ background_action: "force_kill", job_id: "shell_s1" }));

		await execute({ command: "env /bin/bash x.sh" });
		const second = textOf(await execute({ background_action: "force_kill", job_id: "shell_s2" }));

		expect(first).toContain('token "script.sh"');
		expect(second).toContain('token "x.sh"');
	});

	it("parses the darwin etime form into seconds", () => {
		expect(parseElapsed("05:10")).toBe(310);
		expect(parseElapsed("01:02:03")).toBe(3723);
		expect(parseElapsed("2-03:04:05")).toBe(183845);
		expect(parseElapsed("42")).toBe(42);
	});

	it("wakes zero extra times when the poll settles the job during the wall cancel", async () => {
		let release: (() => void) | undefined;
		let statuses = 0;
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => {
			if (args.background_action === "cancel") {
				await new Promise<void>((resolve) => {
					release = resolve;
				});
				return { content: [{ type: "text", text: "cancelled" }] };
			}
			if (args.background_action === "status") {
				statuses += 1;
				return {
					content: [{ type: "text", text: statuses === 1 ? RUNNING("shell_r1") : DONE("shell_r1") }],
				};
			}
			return { content: [{ type: "text", text: STARTED("shell_r1") }] };
		});
		const { execute, sent } = harness({ callTool });

		await execute({ command: "yes", wall_timeout_ms: 1000 });
		// The wall cancel hangs, and the poll tick settles the job in that window.
		await vi.advanceTimersByTimeAsync(1000);
		await vi.advanceTimersByTimeAsync(3000);
		expect(sent).toHaveLength(1);
		expect(String(sent[0].message.details.kind)).toBe("done");

		// The cancel resolves late; the job already left tracking, so no wall wake.
		release?.();
		await vi.advanceTimersByTimeAsync(0);
		expect(sent).toHaveLength(1);
	});

	it("stops tracking a job whose wall retry fails again and reports the lost bound", async () => {
		const callTool = vi.fn(async (_name: string, args: Record<string, unknown>) => {
			if (args.background_action === "cancel") throw new Error("bridge closed");
			return {
				content: [{
					type: "text",
					text: args.background_action === "status" ? RUNNING("shell_t1") : STARTED("shell_t1"),
				}],
			};
		});
		const { execute, sent } = harness({ callTool });

		await execute({ command: "yes", wall_timeout_ms: 1000 });
		await vi.advanceTimersByTimeAsync(1000);
		await vi.advanceTimersByTimeAsync(30000);

		expect(cancelCalls(callTool)).toHaveLength(2);
		expect(sent).toHaveLength(1);
		expect(sent[0].message.content).toContain("wall limit could not be applied");

		// No third retry and no tracking: force_kill finds no record for the job.
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_t1" }));
		expect(body).toContain("no record");
	});

	it("does not revive the jobs widget after shutdown", async () => {
		const callTool = vi.fn(async () => ({ content: [{ type: "text", text: STARTED("shell_u1") }] }));
		const { execute, runShutdown } = harness({ callTool });
		const setWidget = vi.fn();

		// The widget factory holds the only 1 s ticker, so a second registration
		// after shutdown is what a revived ticker would look like.
		await execute({ command: "sleep 30" }, { mode: "tui", ui: { setWidget } });
		const painted = setWidget.mock.calls.length;
		expect(painted).toBeGreaterThan(0);

		await runShutdown();
		expect(setWidget).toHaveBeenLastCalledWith("lean-ctx-jobs", undefined);

		// A later start carries no ui handle, so the cleared one must stay unused.
		await execute({ command: "sleep 31" });
		expect(setWidget).toHaveBeenCalledTimes(painted + 1);
	});

	it("uses the positive pid and marks group NO when our own group is unknown", () => {
		const row = { pid: 4900, ppid: 4400, pgid: 4900, sid: 4900, etimes: 3, args: "sh /tmp/x/k1.sh" };
		const own = { selfPgid: undefined, ancestorPgids: new Set<number>(), verifiable: false };

		// A leader without a known own group must never take the negative form.
		expect(killSignal(row, own)).toEqual({ signal: 4900, group: false });
		expect(psTable([row], own)[1]).toContain("no (pgid unknown)");
	});

	it("reads our own group from the table when the runtime helper throws", () => {
		const host = process as unknown as { getpgrp?: () => number };
		const original = host.getpgrp;
		host.getpgrp = () => {
			throw new Error("boom");
		};
		try {
			const self = { pid: process.pid, ppid: 1, pgid: 4240, sid: 4240, etimes: 1, args: "node pi" };
			const row = { pid: 4910, ppid: 4400, pgid: 4910, sid: 4910, etimes: 3, args: "sh /tmp/x/k2.sh" };
			const own = ownGroups([self, row], process.pid);

			// The chain is known, so the sweep may run, but never as a group signal.
			expect(own.verifiable).toBe(true);
			expect(own.selfPgid).toBeUndefined();
			expect(killSignal(row, own)).toEqual({ signal: 4910, group: false });
		} finally {
			if (original === undefined) delete host.getpgrp;
			else host.getpgrp = original;
		}
	});

	it("refuses the whole sweep when the process table holds no row for this process", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			"  4920  4400  4920  4920       2 sh /tmp/x/k3.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_v1") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/k3.sh" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_v1" }));

		expect(body).toContain("could not verify the process table");
		expect(body).toContain("Nothing was killed");
		expect(body).not.toContain("4920");
		expect(kill).not.toHaveBeenCalled();
		// One table read and no kill step.
		expect(vi.mocked(execFile)).toHaveBeenCalledTimes(1);
	});

	it("kills only the pids the dry run listed when a new matching row appears", async () => {
		const head = "  PID  PPID  PGID   SID ELAPSED COMMAND";
		psReturnsSequence([
			[head, selfRow(4240), "  4820  4400  4820  4820       2 sh /tmp/x/y1.sh"],
			[
				head,
				selfRow(4240),
				"  4820  4400  4820  4820       2 sh /tmp/x/y1.sh",
				"  4830  4400  4830  4830       1 sh /tmp/x/y1.sh",
			],
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_w1") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/y1.sh" });
		const dry = textOf(await execute({ background_action: "force_kill", job_id: "shell_w1" }));
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_w1", confirm: true }));

		expect(dry).not.toContain("4830");
		expect(kill).toHaveBeenCalledTimes(1);
		expect(kill).toHaveBeenCalledWith(-4820, "SIGKILL");
		// The row that appeared after the dry run is named as left alone.
		expect(body).toContain("appeared after the dry run and were left alone: 4830");
		expect(body).toContain("killed 1 of 1");
	});

	it("refuses a confirm without a stored dry run", async () => {
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_w2") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/y2.sh" });
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_w2", confirm: true }));

		expect(body).toContain("no usable dry-run list");
		expect(kill).not.toHaveBeenCalled();
		expect(vi.mocked(execFile)).not.toHaveBeenCalled();
	});

	it("refuses a confirm whose dry-run list is older than 10 minutes", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			"  4860  4400  4860  4860       2 sh /tmp/x/y3.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_w3") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/y3.sh" });
		const dry = textOf(await execute({ background_action: "force_kill", job_id: "shell_w3" }));
		expect(dry).toContain("4860");

		// The clock moves, the timers do not: the list ages out without a poll.
		vi.setSystemTime(Date.now() + 10 * 60 * 1000 + 1);
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_w3", confirm: true }));

		expect(body).toContain("older than 10 minutes");
		expect(kill).not.toHaveBeenCalled();
	});

	it("names every listed pid that the confirm dropped", async () => {
		const head = "  PID  PPID  PGID   SID ELAPSED COMMAND";
		psReturnsSequence([
			[
				head,
				selfRow(4240),
				"  4870  4400  4870  4870       2 sh /tmp/x/y4.sh",
				"  4880  4400  4880  4880       2 sh /tmp/x/y4.sh",
			],
			[head, selfRow(4240), "  4870  4400  4870  4870       3 sh /tmp/x/y4.sh"],
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_w4") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/y4.sh" });
		const dry = textOf(await execute({ background_action: "force_kill", job_id: "shell_w4" }));
		const body = textOf(await execute({ background_action: "force_kill", job_id: "shell_w4", confirm: true }));

		expect(dry).toContain("4880");
		expect(body).toContain("the dry run listed 2 pid(s)");
		expect(body).toContain("killed 1 of 1");
		// The pid that left the table is named with its reason.
		expect(body).toContain("4880 (gone)");
		expect(kill).toHaveBeenCalledTimes(1);
	});

	it("clears the dry-run list after a kill and when the job leaves tracking", async () => {
		psReturns([
			"  PID  PPID  PGID   SID ELAPSED COMMAND",
			selfRow(4240),
			"  4890  4400  4890  4890       2 sh /tmp/x/y5.sh",
		]);
		const kill = vi.spyOn(process, "kill").mockReturnValue(true);
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_w5") }] });
		const { execute } = harness({ callTool });

		await execute({ command: "sh /tmp/x/y5.sh" });
		await execute({ background_action: "force_kill", job_id: "shell_w5" });
		await execute({ background_action: "force_kill", job_id: "shell_w5", confirm: true });
		expect(kill).toHaveBeenCalledTimes(1);

		// The stored list is gone with the kill, so a second confirm refuses.
		const again = textOf(await execute({ background_action: "force_kill", job_id: "shell_w5", confirm: true }));
		expect(again).toContain("no usable dry-run list");
		expect(kill).toHaveBeenCalledTimes(1);
	});

	it("carries the outcome and the exit code on a reply and on a wake", async () => {
		const callTool = vi
			.fn()
			.mockResolvedValueOnce({ content: [{ type: "text", text: STARTED("shell_done") }] })
			.mockResolvedValueOnce({ content: [{ type: "text", text: DONE("shell_done", 2) }] });
		const { execute, sent } = harness({ callTool });

		const started = await execute({ command: "cargo test" });
		expect(started.details).toMatchObject({ outcome: "pending", jobId: "shell_done" });

		await vi.advanceTimersByTimeAsync(2000);
		expect(sent[0].message.details).toMatchObject({ outcome: "failure", exitCode: 2, kind: "done" });
	});

	it("paints the result band from the outcome", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{ type: "text", text: DONE("shell_r1", 0) }],
		});
		const { execute, tool } = harness({ callTool });
		const ok = bandTheme();
		const result = await execute({ background_action: "status", job_id: "shell_r1" });

		const body = drawn(tool().renderResult(result, { expanded: false }, ok.theme));
		expect(ok.tokens).toContain("toolSuccessBg");
		expect(body).toContain("all tests passed");
		expect(body).toContain("rpc_ms=");
		const okBars = ok.fg.filter((call) => call.text === "┃ ");
		expect(okBars.length).toBeGreaterThan(0);
		expect(okBars.every((call) => call.token === "success")).toBe(true);

		callTool.mockResolvedValue({ content: [{ type: "text", text: "ERROR: command timed out" }] });
		const bad = bandTheme();
		const failed = await execute({ background_action: "status", job_id: "shell_r1" });
		const failedBody = drawn(tool().renderResult(failed, { expanded: false }, bad.theme));
		expect(bad.tokens).toContain("toolErrorBg");
		expect(failedBody).toContain("ERROR: command timed out");
		const badBars = bad.fg.filter((call) => call.text === "┃ ");
		expect(badBars.length).toBeGreaterThan(0);
		expect(badBars.every((call) => call.token === "error")).toBe(true);
	});

	it("paints a finished status reply from its exit code", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{ type: "text", text: DONE("shell_e0", 0) }],
		});
		const { execute, tool } = harness({ callTool });

		const ok = bandTheme();
		const done = await execute({ background_action: "status", job_id: "shell_e0" });
		expect(done.details).toMatchObject({ outcome: "success", exitCode: 0 });
		drawn(tool().renderResult(done, { expanded: false }, ok.theme));
		expect(ok.tokens).toContain("toolSuccessBg");

		callTool.mockResolvedValue({ content: [{ type: "text", text: DONE("shell_e3", 3) }] });
		const bad = bandTheme();
		const failed = await execute({ background_action: "status", job_id: "shell_e3" });
		expect(failed.details).toMatchObject({ outcome: "failure", exitCode: 3 });
		drawn(tool().renderResult(failed, { expanded: false }, bad.theme));
		expect(bad.tokens).toContain("toolErrorBg");
	});

	it("paints a running reply pending and truncates a long body", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{
				type: "text",
				text: [RUNNING("shell_r2"), ...Array.from({ length: 8 }, (_v, i) => `line ${i}`)].join("\n"),
			}],
		});
		const { execute, tool } = harness({ callTool });
		const theme = bandTheme();
		const result = await execute({ background_action: "status", job_id: "shell_r2" });

		const body = drawn(tool().renderResult(result, { expanded: false }, theme.theme));
		expect(theme.tokens).toContain("toolPendingBg");
		const pendingBars = theme.fg.filter((call) => call.text === "┃ ");
		expect(pendingBars.length).toBeGreaterThan(0);
		expect(pendingBars.every((call) => call.token === "warning")).toBe(true);
		expect(body).toContain("line 0");
		expect(body).toContain("… 4 more lines");
		expect(body).not.toContain("line 7");

		const open = bandTheme();
		const full = drawn(tool().renderResult(result, { expanded: true }, open.theme));
		expect(full).toContain("line 7");
		expect(full).not.toContain("more lines");
	});

	it("wears one outcome bar per line and paints the body as text", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{ type: "text", text: DONE("shell_bar", 0) }],
		});
		const { execute, tool } = harness({ callTool });
		const theme = bandTheme();
		const result = await execute({ background_action: "status", job_id: "shell_bar" });

		const body = drawn(tool().renderResult(result, { expanded: false }, theme.theme));
		const lines = body.split("\n").filter((line) => line.trim() !== "");
		expect(lines.length).toBeGreaterThan(0);
		for (const line of lines) expect((line.match(/┃/g) ?? []).length).toBe(1);

		const bars = theme.fg.filter((call) => call.text === "┃ ");
		expect(bars.length).toBe(lines.length);
		expect(bars.every((call) => call.token === "success")).toBe(true);
		expect(theme.fg.find((call) => call.text === "all tests passed")?.token).toBe("text");
		expect(theme.fg.find((call) => call.text.startsWith("rpc_ms="))?.token).toBe("dim");
	});

	it("lays the result header out as name, elapsed, result and job id", () => {
		const { tool } = harness({ callTool: vi.fn() });
		const theme = bandTheme();
		const body = drawn(tool().renderResult(
			{
				content: [{ type: "text", text: `${DONE("shell_h1", 0)}\nrpc_ms=12` }],
				details: { name: "unit tests", jobId: "shell_h1", elapsedMs: 65_000, outcome: "success", exitCode: 0 },
			},
			{ expanded: false },
			theme.theme,
		));
		const head = body.split("\n").find((line) => line.includes("unit tests")) ?? "";

		expect(head).toContain(
			`unit tests  ${ICON.clock}  1m05s  ${ICON.done}  completed, exit 0  ${ICON.id}  shell_h1`,
		);
		expect(head.indexOf("unit tests")).toBeLessThan(head.indexOf(ICON.clock));
		expect(head.indexOf(ICON.clock)).toBeLessThan(head.indexOf(ICON.done));
		expect(head.indexOf(ICON.done)).toBeLessThan(head.indexOf(ICON.id));
		expect(theme.fg.find((call) => call.text === "unit tests")?.token).toBe("toolTitle");
		expect(theme.fg.find((call) => call.text === ICON.done)?.token).toBe("success");
		expect(theme.fg.find((call) => call.text === "shell_h1")?.token).toBe("dim");
	});

	it("shows the stopped glyph for a cancelled job and the running glyph for a running one", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{ type: "text", text: "[background:shell_c9 completed exit=130]" }],
		});
		const { execute, tool } = harness({ callTool });

		const stopped = await execute({ background_action: "status", job_id: "shell_c9" });
		expect(stopped.details).toMatchObject({ outcome: "cancelled", exitCode: 130 });
		const stoppedBody = drawn(tool().renderResult(stopped, { expanded: false }, bandTheme().theme));
		expect(stoppedBody).toContain(`${ICON.stopped}  cancelled, exit 130`);
		expect(stoppedBody).not.toContain(ICON.fail);

		callTool.mockResolvedValue({ content: [{ type: "text", text: RUNNING("shell_r9") }] });
		const running = await execute({ background_action: "status", job_id: "shell_r9" });
		const runningBody = drawn(tool().renderResult(running, { expanded: false }, bandTheme().theme));
		expect(runningBody).toContain(`${ICON.running}  running`);
		expect(runningBody).not.toContain(ICON.done);
	});

	it("lays the follow-up header out from the carried fields", () => {
		const { renderer } = harness({ callTool: vi.fn() });
		const theme = bandTheme();
		const body = drawn(renderer()(
			{
				content: "lean-ctx background job shell_m1 finished.\n$ make\nok",
				details: { outcome: "failure", exitCode: 3, name: "unit tests", jobId: "shell_m1", elapsedMs: 3000 },
			},
			{ expanded: false },
			theme.theme,
		));
		const head = body.split("\n").find((line) => line.includes("unit tests")) ?? "";

		expect(head).toContain(
			`unit tests  ${ICON.clock}  3s  ${ICON.fail}  failed, exit 3  ${ICON.id}  shell_m1`,
		);
	});

	it("leaves the call line to Pi's default renderer", () => {
		const { tool } = harness({ callTool: vi.fn() });
		expect(tool()).not.toHaveProperty("renderCall");
	});

	it("paints the follow-up message from the carried outcome", () => {
		const { renderer, rendererType } = harness({ callTool: vi.fn() });
		expect(rendererType()).toBe("lean-ctx-background");

		const failed = bandTheme();
		const failedBody = drawn(renderer()(
			{
				content: [
					"lean-ctx background job shell_x finished.",
					"$ make",
					"BUILD FAILED",
					...Array.from({ length: 6 }, (_v, i) => `warning ${i}`),
				].join("\n"),
				details: { outcome: "failure" },
			},
			{ expanded: false },
			failed.theme,
		));
		expect(failed.tokens).toContain("toolErrorBg");
		expect(failedBody).toContain("BUILD FAILED");
		const lines = failedBody.split("\n").filter((line) => line.trim() !== "");
		for (const line of lines) expect((line.match(/┃/g) ?? []).length).toBe(1);
		const bars = failed.fg.filter((call) => call.text === "┃ ");
		expect(bars.length).toBe(lines.length);
		expect(bars.every((call) => call.token === "error")).toBe(true);
		expect(failed.fg.find((call) => call.text === " $ make")?.token).toBe("text");
		expect(failed.fg.find((call) => /^ \+\d+ lines$/.test(call.text))?.token).toBe("dim");

		const done = bandTheme();
		drawn(renderer()(
			{ content: "lean-ctx background job shell_y finished.\n$ make", details: { outcome: "success" } },
			{ expanded: false },
			done.theme,
		));
		expect(done.tokens).toContain("toolSuccessBg");
	});

	it("pads both ctx_bg blocks by one banded row and leaves the rows unbarred", async () => {
		const callTool = vi.fn().mockResolvedValue({
			content: [{ type: "text", text: `${DONE("shell_pad", 0)}\nrpc_ms=12` }],
		});
		const { execute, tool, renderer } = harness({ callTool });

		const message = bandTheme();
		const rows = renderer()(
			{
				content: "lean-ctx background job shell_pad finished.\n$ make\nok",
				details: { outcome: "success" },
			},
			{ expanded: false },
			message.theme,
		).render(80);
		expect(rows.length).toBe(5);
		expect(rows[0].trim()).toBe("");
		expect(rows[rows.length - 1].trim()).toBe("");
		const messagePad = message.bands.filter((band) => band.text.trim() === "" && band.text.length === 80);
		expect(messagePad).toHaveLength(2);
		expect(messagePad.every((band) => band.token === "toolSuccessBg")).toBe(true);
		for (const row of [rows[0], rows[rows.length - 1]]) expect((row.match(/┃/g) ?? []).length).toBe(0);
		for (const row of rows.slice(1, -1)) expect((row.match(/┃/g) ?? []).length).toBe(1);

		const blockTheme = bandTheme();
		const result = await execute({ background_action: "status", job_id: "shell_pad" });
		const block = tool().renderResult(result, { expanded: false }, blockTheme.theme).render(80);
		expect(block[0].trim()).toBe("");
		expect(block[block.length - 1].trim()).toBe("");
		expect(block.filter((row) => row.trim() === "")).toHaveLength(2);
		const blockPad = blockTheme.bands.filter((band) => band.text.trim() === "" && band.text.length === 80);
		expect(blockPad).toHaveLength(2);
		expect(blockPad.every((band) => band.token === "toolSuccessBg")).toBe(true);
		for (const row of block.slice(1, -1)) expect((row.match(/┃/g) ?? []).length).toBe(1);
	});
});

describe("boundMessageBody", () => {
	const lines = (count: number, text = (index: number) => `line ${index + 1}`) =>
		Array.from({ length: count }, (_v, i) => text(i));

	it("keeps the first two lines and the last four of a long body", () => {
		const kept = boundMessageBody(lines(200).join("\n")).split("\n");

		expect(kept[0]).toBe("line 1");
		expect(kept[1]).toBe("line 2");
		expect(kept.slice(-4)).toEqual(["line 197", "line 198", "line 199", "line 200"]);
		expect(kept).toHaveLength(7);
	});

	it("returns a body of six lines or fewer byte-identical", () => {
		const short = "line 1\nline 2\nline 3\nline 4";

		expect(boundMessageBody(short)).toBe(short);
		expect(boundMessageBody(lines(6).join("\n"))).toBe(lines(6).join("\n"));
	});

	it("states the hidden line count in the marker", () => {
		const body = boundMessageBody(lines(200).join("\n"));
		const markers = body.split("\n").filter((line) => line.includes("lines hidden"));

		expect(markers).toEqual(["… 194 lines hidden"]);
	});

	it("drops whole middle lines to stay inside the char cap", () => {
		const wide = lines(200, (i) => `${i + 1} ${"x".repeat(40)}`).join("\n");
		const body = boundMessageBody(wide);
		const kept = body.split("\n");

		expect(body.length).toBeLessThanOrEqual(1300);
		expect(kept[0]).toMatch(/^1 x+$/);
		expect(kept.at(-1)).toMatch(/^200 x+$/);
		expect(kept).toContain(`… ${200 - (kept.length - 1)} lines hidden`);
		// Every kept line is still a whole line of the input, in its own order.
		for (const line of kept.filter((entry) => !entry.includes("lines hidden"))) expect(wide.split("\n")).toContain(line);
	});
});

describe("classifyOutcome", () => {
	it("reads a terminal exit line", () => {
		expect(classifyOutcome("[background:shell_a1 completed exit=0]\nall tests passed", "status")).toBe("success");
		expect(classifyOutcome("[background:shell_a1 completed exit 0]", "status")).toBe("success");
		expect(classifyOutcome("[background:shell_a1 completed exit=1]\nboom", "status")).toBe("failure");
		expect(classifyOutcome("[background:shell_a1 completed exit=130]", "status")).toBe("cancelled");
	});

	it("keeps a running or cancelled job pending", () => {
		expect(classifyOutcome(RUNNING("shell_a1"), "status")).toBe("pending");
		expect(classifyOutcome("cancel requested for shell_a1", "cancel")).toBe("pending");
		expect(classifyOutcome("", "start")).toBe("pending");
	});

	it("reports a failure marker whatever the exit line says", () => {
		expect(classifyOutcome("ERROR: command timed out", "status")).toBe("failure");
		expect(classifyOutcome("job not found or expired", "status")).toBe("failure");
		expect(classifyOutcome("ERROR: command timed out\nexit=0", "status")).toBe("failure");
	});
});
