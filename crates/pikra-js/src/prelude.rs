//! JavaScript prelude evaluated inside each QuickJS extension VM before extensions load.

pub(crate) const PRELUDE_SOURCE: &str = r#"(function () {
	"use strict";

	const SUPPORTED_HOOKS = new Set([
		"prepare_request",
		"transform_context",
		"before_tool_call",
		"after_tool_call",
		"finish_turn",
	]);

	const tools = new Map();
	const hooks = new Map();
	for (const name of SUPPORTED_HOOKS) {
		hooks.set(name, []);
	}

	const defaultParameters = { type: "object", properties: {} };

	function registerTool(spec) {
		if (!spec || typeof spec !== "object" || Array.isArray(spec)) {
			throw new TypeError("pikra.registerTool(spec) expects an object");
		}
		if (typeof spec.name !== "string" || spec.name.trim() === "") {
			throw new TypeError("pikra.registerTool: 'name' must be a non-empty string");
		}
		if (spec.description !== undefined && typeof spec.description !== "string") {
			throw new TypeError("pikra.registerTool: 'description' must be a string");
		}
		if (typeof spec.execute !== "function") {
			throw new TypeError("pikra.registerTool: 'execute' must be a function");
		}
		const parameters = spec.parameters === undefined ? defaultParameters : spec.parameters;
		if (
			(typeof parameters !== "object" || parameters === null || Array.isArray(parameters)) &&
			typeof parameters !== "boolean"
		) {
			throw new TypeError("pikra.registerTool: 'parameters' must be a JSON Schema object or boolean");
		}
		const executionMode = spec.executionMode !== undefined ? spec.executionMode : spec.execution_mode;
		if (
			executionMode !== undefined &&
			executionMode !== "parallel" &&
			executionMode !== "sequential"
		) {
			throw new TypeError(
				"pikra.registerTool: 'executionMode' must be 'parallel' or 'sequential'"
			);
		}
		// Validate schema is JSON-serializable immediately during registration.
		JSON.stringify(parameters);

		tools.set(spec.name, {
			name: spec.name,
			description: spec.description || "",
			parameters,
			executionMode: executionMode || "parallel",
			execute: spec.execute,
		});
	}

	function on(event, handler) {
		if (typeof event !== "string" || !SUPPORTED_HOOKS.has(event)) {
			throw new TypeError("pikra.on: unsupported hook event '" + String(event) + "'");
		}
		if (typeof handler !== "function") {
			throw new TypeError("pikra.on: handler for '" + event + "' must be a function");
		}
		hooks.get(event).push(handler);
	}

	const pikraApi = Object.freeze({
		registerTool,
		on,
	});

	if (typeof globalThis.console === "undefined") {
		const noop = () => {};
		Object.defineProperty(globalThis, "console", {
			value: Object.freeze({ log: noop, info: noop, warn: noop, error: noop, debug: noop }),
			enumerable: true,
			configurable: true,
		});
	}

	Object.defineProperty(globalThis, "pikra", {
		value: pikraApi,
		enumerable: true,
		writable: false,
		configurable: false,
	});

	function normalizeToolReturn(value) {
		if (typeof value === "string") {
			return { content: [{ type: "text", text: value }] };
		}
		if (value === undefined || value === null) {
			return { content: [{ type: "text", text: "" }] };
		}
		if (typeof value === "object" && !Array.isArray(value)) {
			const out = {};
			if (value.content !== undefined) {
				out.content = value.content;
			}
			if (value.details !== undefined) {
				out.details = value.details;
			}
			if (value.structuredContent !== undefined) {
				out.structuredContent = value.structuredContent;
			} else if (value.structured_content !== undefined) {
				out.structuredContent = value.structured_content;
			}
			if (value.isError !== undefined) {
				out.isError = Boolean(value.isError);
			} else if (value.is_error !== undefined) {
				out.isError = Boolean(value.is_error);
			}
			if (value.terminate !== undefined) {
				out.terminate = Boolean(value.terminate);
			}
			return out;
		}
		return { content: [{ type: "text", text: String(value) }] };
	}

	function inspect() {
		const toolList = [];
		for (const tool of tools.values()) {
			toolList.push({
				name: tool.name,
				description: tool.description,
				parameters: tool.parameters,
				executionMode: tool.executionMode,
			});
		}
		const activeHooks = [];
		for (const [name, list] of hooks.entries()) {
			if (list.length > 0) {
				activeHooks.push(name);
			}
		}
		return JSON.stringify({ tools: toolList, hooks: activeHooks });
	}

	async function invokeTool(name, toolCallId, argsJson) {
		const tool = tools.get(name);
		if (!tool) {
			throw new Error("JS tool '" + name + "' is not registered");
		}
		const args = JSON.parse(argsJson);
		const rawResult = await tool.execute(toolCallId, args);
		return JSON.stringify(normalizeToolReturn(rawResult));
	}

	async function invokeHook(event, payloadJson) {
		const listeners = hooks.get(event);
		if (!listeners || listeners.length === 0) {
			return undefined;
		}

		if (event === "prepare_request") {
			let snapshot = JSON.parse(payloadJson);
			let changed = false;
			const merged = {};
			for (const listener of listeners) {
				const res = await listener(snapshot);
				if (res && typeof res === "object") {
					const sp = res.systemPrompt !== undefined ? res.systemPrompt : res.system_prompt;
					if (sp !== undefined) {
						merged.systemPrompt = sp;
						snapshot = Object.assign({}, snapshot, { systemPrompt: sp });
						changed = true;
					}
					const tl = res.thinkingLevel !== undefined ? res.thinkingLevel : res.thinking_level;
					if (tl !== undefined) {
						merged.thinkingLevel = tl;
						snapshot = Object.assign({}, snapshot, { thinkingLevel: tl });
						changed = true;
					}
					if (res.fast !== undefined) {
						merged.fast = res.fast;
						snapshot = Object.assign({}, snapshot, { fast: res.fast });
						changed = true;
					}
					if (res.tools !== undefined) {
						merged.tools = res.tools;
						snapshot = Object.assign({}, snapshot, { tools: res.tools });
						changed = true;
					}
					if (res.messages !== undefined) {
						merged.messages = res.messages;
						snapshot = Object.assign({}, snapshot, { messages: res.messages });
						changed = true;
					}
					if (res.model !== undefined) {
						merged.model = res.model;
						snapshot = Object.assign({}, snapshot, { model: res.model });
						changed = true;
					}
				}
			}
			return changed ? JSON.stringify(merged) : undefined;
		}

		if (event === "transform_context") {
			let messages = JSON.parse(payloadJson);
			for (const listener of listeners) {
				const res = await listener(messages);
				if (Array.isArray(res)) {
					messages = res;
				}
			}
			return JSON.stringify(messages);
		}

		if (event === "before_tool_call") {
			const payload = JSON.parse(payloadJson);
			let currentArgs = payload.args;
			let changed = false;
			let argsOverride = undefined;
			for (const listener of listeners) {
				const res = await listener({
					toolCall: payload.toolCall,
					args: currentArgs,
					assistantMessage: payload.assistantMessage,
				});
				if (res && typeof res === "object") {
					const ov = res.argsOverride !== undefined ? res.argsOverride : res.args_override;
					if (ov !== undefined) {
						currentArgs = ov;
						argsOverride = ov;
						changed = true;
					}
					if (res.block) {
						return JSON.stringify({
							block: true,
							reason: res.reason !== undefined ? String(res.reason) : undefined,
							terminate: Boolean(res.terminate),
							argsOverride,
						});
					}
				}
			}
			if (!changed) {
				return undefined;
			}
			return JSON.stringify({
				block: false,
				terminate: false,
				argsOverride,
			});
		}

		if (event === "after_tool_call") {
			const payload = JSON.parse(payloadJson);
			let currentResult = payload.result;
			let currentIsError = Boolean(payload.isError);
			let changed = false;
			const merged = {};
			for (const listener of listeners) {
				const res = await listener({
					toolCall: payload.toolCall,
					args: payload.args,
					result: currentResult,
					isError: currentIsError,
				});
				if (res && typeof res === "object") {
					if (res.content !== undefined) {
						merged.content = res.content;
						const normalizedContent =
							typeof res.content === "string"
								? [{ type: "text", text: res.content }]
								: res.content;
						currentResult = Object.assign({}, currentResult, { content: normalizedContent });
						changed = true;
					}
					if (res.details !== undefined) {
						merged.details = res.details;
						currentResult = Object.assign({}, currentResult, { details: res.details });
						changed = true;
					}
					const sc =
						res.structuredContent !== undefined
							? res.structuredContent
							: res.structured_content;
					if (sc !== undefined) {
						merged.structuredContent = sc;
						currentResult = Object.assign({}, currentResult, { structuredContent: sc });
						changed = true;
					}
					const ie = res.isError !== undefined ? res.isError : res.is_error;
					if (ie !== undefined) {
						const boolErr = Boolean(ie);
						merged.isError = boolErr;
						currentIsError = boolErr;
						currentResult = Object.assign({}, currentResult, { isError: boolErr });
						changed = true;
					}
					if (res.terminate !== undefined) {
						const boolTerm = Boolean(res.terminate);
						merged.terminate = boolTerm;
						currentResult = Object.assign({}, currentResult, { terminate: boolTerm });
						changed = true;
					}
				}
			}
			return changed ? JSON.stringify(merged) : undefined;
		}

		if (event === "finish_turn") {
			const payload = JSON.parse(payloadJson);
			let decision = undefined;
			for (const listener of listeners) {
				const res = await listener(payload);
				const action =
					typeof res === "string"
						? res
						: res && typeof res === "object" && typeof res.action === "string"
							? res.action
							: undefined;
				if (action === "end") {
					return JSON.stringify({ action: "end" });
				}
				if (action === "continue") {
					decision = "continue";
				}
			}
			return decision !== undefined ? JSON.stringify({ action: decision }) : undefined;
		}

		return undefined;
	}

	return {
		api: pikraApi,
		inspect,
		invokeTool,
		invokeHook,
	};
})"#;
