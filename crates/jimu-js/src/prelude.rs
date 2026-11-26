//! JavaScript prelude evaluated inside each QuickJS extension VM before extensions load.

pub(crate) const PRELUDE_SOURCE: &str = r#"(function (onUiInvalidate) {
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
	const uiRenderHandlers = [];
	const uiThemeOverrides = Object.create(null);

	function isPlainObject(value) {
		return value !== null && typeof value === "object" && !Array.isArray(value);
	}

	function isThenable(value) {
		return (
			value !== null &&
			(typeof value === "object" || typeof value === "function") &&
			typeof value.then === "function"
		);
	}

	function normalizeUiChildren(raw, out) {
		if (raw === undefined || raw === null || raw === false) {
			return;
		}
		if (Array.isArray(raw)) {
			for (const item of raw) {
				normalizeUiChildren(item, out);
			}
			return;
		}
		if (isThenable(raw)) {
			throw new TypeError("UI node children cannot contain a Promise or thenable");
		}
		if (typeof raw === "string") {
			out.push({ type: "text", text: raw });
			return;
		}
		if (isPlainObject(raw)) {
			out.push(raw);
			return;
		}
		throw new TypeError("UI node child must be a UiNode object or string");
	}

	function uiText(text, style) {
		const node = { type: "text", text: String(text) };
		if (isPlainObject(style)) {
			if (style.color !== undefined) node.color = style.color;
			if (style.bg !== undefined) node.bg = style.bg;
			if (style.bold !== undefined) node.bold = Boolean(style.bold);
			if (style.dim !== undefined) node.dim = Boolean(style.dim);
			if (style.italic !== undefined) node.italic = Boolean(style.italic);
			if (style.underline !== undefined) node.underline = Boolean(style.underline);
		}
		return node;
	}

	function buildBoxNode(forcedDirection, propsOrChildren, childrenArg) {
		let props = {};
		let rawChildren = undefined;

		if (Array.isArray(propsOrChildren)) {
			rawChildren = propsOrChildren;
		} else if (typeof propsOrChildren === "string") {
			rawChildren = [propsOrChildren];
		} else if (isPlainObject(propsOrChildren)) {
			if (typeof propsOrChildren.type === "string" && childrenArg === undefined) {
				rawChildren = [propsOrChildren];
			} else {
				props = propsOrChildren;
				rawChildren = childrenArg !== undefined ? childrenArg : propsOrChildren.children;
			}
		} else if (propsOrChildren === undefined || propsOrChildren === null) {
			rawChildren = childrenArg;
		} else {
			throw new TypeError("jimu.ui.box/row expects a props object, children array, or UiNode");
		}

		const node = { type: "box" };
		if (forcedDirection !== undefined) {
			node.direction = forcedDirection;
		} else if (props.direction !== undefined) {
			node.direction = props.direction;
		}
		if (props.gap !== undefined) node.gap = props.gap;
		if (props.border !== undefined) node.border = props.border;
		const borderColor = props.border_color !== undefined ? props.border_color : props.borderColor;
		if (borderColor !== undefined) node.border_color = borderColor;
		if (props.bg !== undefined) node.bg = props.bg;
		const paddingLeft = props.padding_left !== undefined ? props.padding_left : props.paddingLeft;
		if (paddingLeft !== undefined) node.padding_left = paddingLeft;
		const paddingRight =
			props.padding_right !== undefined ? props.padding_right : props.paddingRight;
		if (paddingRight !== undefined) node.padding_right = paddingRight;

		const children = [];
		normalizeUiChildren(rawChildren, children);
		node.children = children;
		return node;
	}

	function uiBox(propsOrChildren, children) {
		return buildBoxNode(undefined, propsOrChildren, children);
	}

	function uiRow(childrenOrProps, children) {
		return buildBoxNode("row", childrenOrProps, children);
	}

	function uiDivider(style) {
		const node = { type: "divider" };
		if (typeof style === "string") {
			node.ch = style;
		} else if (isPlainObject(style)) {
			if (style.ch !== undefined) node.ch = String(style.ch);
			if (style.color !== undefined) node.color = style.color;
		}
		return node;
	}

	function uiNative() {
		return { type: "native" };
	}

	function uiTheme(overrides) {
		if (!isPlainObject(overrides)) {
			throw new TypeError("jimu.ui.theme(overrides) expects a plain object");
		}
		const keys = Object.keys(overrides);
		for (const key of keys) {
			if (key.trim() === "") {
				throw new TypeError("jimu.ui.theme: theme token key must be a non-empty string");
			}
			const val = overrides[key];
			if (typeof val !== "string") {
				throw new TypeError("jimu.ui.theme: value for '" + key + "' must be a string");
			}
			uiThemeOverrides[key] = val;
		}
	}

	function uiInvalidate() {
		if (typeof onUiInvalidate === "function") {
			onUiInvalidate();
		}
	}

	const defaultParameters = { type: "object", properties: {} };

	function registerTool(spec) {
		if (!spec || typeof spec !== "object" || Array.isArray(spec)) {
			throw new TypeError("jimu.registerTool(spec) expects an object");
		}
		if (typeof spec.name !== "string" || spec.name.trim() === "") {
			throw new TypeError("jimu.registerTool: 'name' must be a non-empty string");
		}
		if (spec.description !== undefined && typeof spec.description !== "string") {
			throw new TypeError("jimu.registerTool: 'description' must be a string");
		}
		if (typeof spec.execute !== "function") {
			throw new TypeError("jimu.registerTool: 'execute' must be a function");
		}
		const parameters = spec.parameters === undefined ? defaultParameters : spec.parameters;
		if (
			(typeof parameters !== "object" || parameters === null || Array.isArray(parameters)) &&
			typeof parameters !== "boolean"
		) {
			throw new TypeError("jimu.registerTool: 'parameters' must be a JSON Schema object or boolean");
		}
		const executionMode = spec.executionMode !== undefined ? spec.executionMode : spec.execution_mode;
		if (
			executionMode !== undefined &&
			executionMode !== "parallel" &&
			executionMode !== "sequential"
		) {
			throw new TypeError(
				"jimu.registerTool: 'executionMode' must be 'parallel' or 'sequential'"
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
		if (event === "ui.render") {
			if (typeof handler !== "function") {
				throw new TypeError("jimu.on: handler for 'ui.render' must be a function");
			}
			uiRenderHandlers.push(handler);
			return;
		}
		if (typeof event !== "string" || !SUPPORTED_HOOKS.has(event)) {
			throw new TypeError("jimu.on: unsupported hook event '" + String(event) + "'");
		}
		if (typeof handler !== "function") {
			throw new TypeError("jimu.on: handler for '" + event + "' must be a function");
		}
		hooks.get(event).push(handler);
	}

	const uiApi = Object.freeze({
		text: uiText,
		box: uiBox,
		row: uiRow,
		divider: uiDivider,
		native: uiNative,
		theme: uiTheme,
		invalidate: uiInvalidate,
	});

	const jimuApi = Object.freeze({
		registerTool,
		on,
		ui: uiApi,
	});

	if (typeof globalThis.console === "undefined") {
		const noop = () => {};
		Object.defineProperty(globalThis, "console", {
			value: Object.freeze({ log: noop, info: noop, warn: noop, error: noop, debug: noop }),
			enumerable: true,
			configurable: true,
		});
	}

	Object.defineProperty(globalThis, "jimu", {
		value: jimuApi,
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
		return JSON.stringify({
			tools: toolList,
			hooks: activeHooks,
			hasUiRender: uiRenderHandlers.length > 0,
			uiThemeOverrides: Object.assign({}, uiThemeOverrides),
		});
	}

	// Synchronous onion middleware chain in registration/load order:
	// the first registered handler executes outermost; calling `next(e)` enters the next handler;
	// the innermost `next(e)` returns `{ type: "native" }`.
	function invokeUiRender(component, propsJson) {
		if (uiRenderHandlers.length === 0) {
			return JSON.stringify({ type: "native" });
		}
		const parsedProps = JSON.parse(propsJson);
		const initialEvent = Object.freeze({
			component,
			props: isPlainObject(parsedProps) ? Object.freeze(parsedProps) : parsedProps,
		});

		function dispatch(index, event) {
			if (index >= uiRenderHandlers.length) {
				return { type: "native" };
			}
			const handler = uiRenderHandlers[index];
			let calledNext = false;
			let lastNextResult = undefined;
			const next = function (overrideEvent) {
				const nextEvent = overrideEvent === undefined ? event : overrideEvent;
				lastNextResult = dispatch(index + 1, nextEvent);
				calledNext = true;
				return lastNextResult;
			};
			const res = handler(event, next);
			if (isThenable(res)) {
				throw new TypeError(
					"ui.render handler must be synchronous and cannot return a Promise/thenable"
				);
			}
			if (res === undefined || res === null) {
				return calledNext ? lastNextResult : dispatch(index + 1, event);
			}
			if (typeof res === "string") {
				return { type: "text", text: res };
			}
			if (typeof res !== "object" || Array.isArray(res)) {
				throw new TypeError(
					"ui.render handler must return a UiNode object, string, or undefined/null"
				);
			}
			return res;
		}

		const result = dispatch(0, initialEvent);
		return JSON.stringify(result);
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
		api: jimuApi,
		inspect,
		invokeTool,
		invokeHook,
		invokeUiRender,
	};
})"#;
