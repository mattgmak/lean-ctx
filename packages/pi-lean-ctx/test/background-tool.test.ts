import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { registerBackgroundTool } from "../extensions/background-tool.js";

type SentMessage = { customType: string; content: string; details: Record<string, unknown> };
type SentOptions = { triggerTurn?: boolean; deliverAs?: string };

const RUNNING = (id: string, output = "step 1 of 10") => `[background:${id} running]\n${output}`;
const DONE = (id: string, code = 0) => `[background:${id} completed exit=${code}]\nall tests passed`;
const STARTED = (id: string) =>
	`[background:${id} started — use ctx_shell(background_action="status", job_id="${id}") to poll or background_action="cancel" to stop it]`;

type Execute = (toolCallId: string, params: Record<string, unknown>, signal?: AbortSignal) => Promise<{ content: Array<{ text: string }> }>;

interface Harness {
	sent: Array<{ message: SentMessage; options: SentOptions }>;
	shutdown: Array<() => Promise<void>>;
	callTool: ReturnType<typeof vi.fn>;
	execute: (params: Record<string, unknown>) => Promise<{ content: Array<{ text: string }> }>;
	runShutdown: () => Promise<void>;
	runStart: (ctx: unknown) => Promise<void>;
}

function harness(
	bridge: { callTool: ReturnType<typeof vi.fn> } | undefined,
): Harness {
	const sent: Harness["sent"] = [];
	const shutdown: Harness["shutdown"] = [];
	const start: Array<(event: unknown, ctx: unknown) => Promise<void>> = [];
	let registered: { execute: Execute } | undefined;

	registerBackgroundTool({
		pi: {
			sendMessage: (message: SentMessage, options: SentOptions) => {
				sent.push({ message, options });
			},
			registerMessageRenderer: () => {},
			on: (event: string, handler: (event: unknown, ctx: unknown) => Promise<void>) => {
				if (event === "session_shutdown") shutdown.push(handler as () => Promise<void>);
				if (event === "session_start") start.push(handler);
			},
		} as never,
		registerTool: ((def: unknown) => {
			registered = def as { execute: Execute };
		}) as never,
		getBridge: () => bridge as never,
	});

	return {
		sent,
		shutdown,
		callTool: bridge?.callTool ?? vi.fn(),
		execute: (params, ctx) => registered!.execute("call-1", params, undefined, undefined, ctx as never),
		runShutdown: async () => {
			for (const handler of shutdown) await handler();
		},
		runStart: async (ctx: unknown) => {
			for (const handler of start) await handler({ type: "session_start", reason: "startup" }, ctx);
		},
	};
}

const textOf = (result: { content: Array<{ text: string }> }) => result.content[0].text;

describe("ctx_bg", () => {
	beforeEach(() => {
		vi.useFakeTimers();
	});

	afterEach(() => {
		vi.useRealTimers();
	});

	it("starts a job in background mode and reports the job id", async () => {
		const callTool = vi.fn().mockResolvedValue({ content: [{ type: "text", text: STARTED("shell_a1") }] });
		const { execute } = harness({ callTool });

		const result = await execute({ command: "cargo test" });

		expect(callTool).toHaveBeenCalledWith("ctx_shell", { command: "cargo test", run_in_background: true });
		expect(textOf(result)).toContain("shell_a1");
		expect(textOf(result)).toContain("follow-up message");
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
		expect(callTool).toHaveBeenLastCalledWith("ctx_shell", { background_action: "status", job_id: "shell_f1" });

		await execute({ background_action: "cancel", job_id: "shell_f1" });
		expect(callTool).toHaveBeenLastCalledWith("ctx_shell", { background_action: "cancel", job_id: "shell_f1" });
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
		expect(lines[0]).toContain("shell_w1 cargo test");
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
});
