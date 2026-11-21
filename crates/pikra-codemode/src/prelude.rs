//! JavaScript prelude evaluated inside each QuickJS VM before the user script runs.

use crate::types::{
    MAX_OUTPUT_CHARS, MAX_OUTPUT_ITEMS, MAX_STORE_TOTAL_CHARS, MAX_STORE_VALUE_CHARS,
};

pub(crate) fn build_prelude_source() -> String {
    PRELUDE_TEMPLATE
        .replace(
            "__MAX_STORE_VALUE_CHARS__",
            &MAX_STORE_VALUE_CHARS.to_string(),
        )
        .replace(
            "__MAX_STORE_TOTAL_CHARS__",
            &MAX_STORE_TOTAL_CHARS.to_string(),
        )
        .replace("__MAX_OUTPUT_CHARS__", &MAX_OUTPUT_CHARS.to_string())
        .replace("__MAX_OUTPUT_ITEMS__", &MAX_OUTPUT_ITEMS.to_string())
}

const PRELUDE_TEMPLATE: &str = r#"(function (bridge, toolsJson, storeJson) {
	"use strict";

	// Freeze every object reachable from the built-in globals plus hidden intrinsics,
	// preserving the ability for subclasses/instances to assign overridable properties.
	(function lockdown() {
		const OVERRIDABLE = new Set(["constructor", "name", "message", "toString", "toLocaleString", "valueOf", "toJSON"]);
		const seen = new Set([globalThis]);
		const queue = [];
		const add = (value) => {
			if ((typeof value === "object" && value !== null) || typeof value === "function") {
				if (!seen.has(value)) {
					seen.add(value);
					queue.push(value);
				}
			}
		};

		function allowOverride(object, key, value, enumerable) {
			const { get, set } = Object.getOwnPropertyDescriptor(
				{
					get accessor() {
						return value;
					},
					set accessor(next) {
						if (this === object) {
							throw new TypeError("Cannot assign to read only property '" + String(key) + "' of a built-in");
						}
						if ((typeof this !== "object" || this === null) && typeof this !== "function") return;
						Object.defineProperty(this, key, { value: next, writable: true, enumerable: true, configurable: true });
					},
				},
				"accessor",
			);
			Object.defineProperty(object, key, { get, set, enumerable, configurable: false });
			add(get);
			add(set);
		}

		for (const key of Reflect.ownKeys(globalThis)) {
			const descriptor = Object.getOwnPropertyDescriptor(globalThis, key);
			add(descriptor.value);
			add(descriptor.get);
			add(descriptor.set);
			if ("value" in descriptor && descriptor.configurable) {
				Object.defineProperty(globalThis, key, { writable: false, configurable: false });
			}
		}
		add(Object.getPrototypeOf(function* () {}));
		add(Object.getPrototypeOf(async function () {}));
		add(Object.getPrototypeOf(async function* () {}));
		add(Object.getPrototypeOf(Int8Array));
		add(Object.getPrototypeOf([][Symbol.iterator]()));
		add(Object.getPrototypeOf(new Map()[Symbol.iterator]()));
		add(Object.getPrototypeOf(new Set()[Symbol.iterator]()));
		add(Object.getPrototypeOf(""[Symbol.iterator]()));
		add(Object.getPrototypeOf(/a/[Symbol.matchAll]("")));

		while (queue.length > 0) {
			const object = queue.pop();
			add(Object.getPrototypeOf(object));
			const descriptors = Object.getOwnPropertyDescriptors(object);
			for (const key of Reflect.ownKeys(descriptors)) {
				const descriptor = descriptors[key];
				if ("value" in descriptor) {
					add(descriptor.value);
					if (
						descriptor.writable &&
						descriptor.configurable &&
						(object === Object.prototype || OVERRIDABLE.has(key))
					) {
						allowOverride(object, key, descriptor.value, descriptor.enumerable);
					}
				} else {
					add(descriptor.get);
					add(descriptor.set);
				}
			}
			Object.freeze(object);
		}
	})();

	const stringify = JSON.stringify;
	const parse = JSON.parse;
	const promiseThen = Promise.prototype.then;
	const ErrorCtor = Error;
	const TypeErrorCtor = TypeError;
	const RangeErrorCtor = RangeError;
	const pending = new Map();
	let nextId = 1;
	let finished = false;
	const EXIT_SENTINEL = Object.freeze({ [Symbol("pikra.codemode.exit")]: true });

	function isOom(error) {
		try {
			return (
				error !== null &&
				typeof error === "object" &&
				error.name === "InternalError" &&
				typeof error.message === "string" &&
				error.message.toLowerCase().includes("out of memory")
			);
		} catch {
			return false;
		}
	}

	function done(ok, payload, writes) {
		if (finished) return;
		finished = true;
		bridge("done", ok, payload, writes);
	}

	function doneOom(message) {
		if (finished) return;
		finished = true;
		bridge("oom", message || "out of memory");
	}

	function serialize(value) {
		return value === undefined ? undefined : stringify(value);
	}

	function errorText(error) {
		const head = error.message ? error.name + ": " + error.message : String(error.name);
		const frames =
			typeof error.stack === "string"
				? error.stack.split("\n").filter((line) => line.trim() && !line.includes("codemode-prelude.js"))
				: [];
		return [head, ...frames].join("\n");
	}

	function format(value) {
		const t = typeof value;
		if (t === "string") return value;
		if (value instanceof ErrorCtor) return errorText(value);
		if (t === "number" || t === "bigint" || t === "boolean" || t === "undefined" || t === "symbol") {
			return String(value);
		}
		try {
			const json = stringify(value);
			return json === undefined ? String(value) : json;
		} catch {
			return String(value);
		}
	}

	function describeError(error) {
		try {
			if (error instanceof ErrorCtor) {
				return stringify({
					name: String(error.name),
					message: String(error.message),
					stack: errorText(error),
				});
			}
			return stringify({ message: String(format(error)) });
		} catch {
			return stringify({ message: "The script threw a value that cannot be described" });
		}
	}

	function reportFailure(error) {
		if (error === EXIT_SENTINEL) return;
		if (isOom(error)) {
			doneOom(String(error.message));
			return;
		}
		done(false, describeError(error));
	}

	function caller(rawName) {
		return (...args) =>
			new Promise((resolve, reject) => {
				if (finished) {
					reject(new ErrorCtor("Execution already finished"));
					return;
				}
				let json;
				try {
					json = serialize(args[0]);
				} catch (error) {
					reject(error);
					return;
				}
				const id = nextId++;
				pending.set(id, { resolve, reject });
				bridge("call", id, rawName, json);
			});
	}

	const tools = Object.create(null);
	const allTools = [];
	for (const { name, rawName, description } of parse(toolsJson)) {
		const fn = caller(rawName);
		if (!(name in tools)) {
			tools[name] = fn;
			allTools.push(Object.freeze({ name, rawName, description }));
		}
		if (!(rawName in tools)) {
			tools[rawName] = fn;
		}
	}
	Object.freeze(tools);
	Object.freeze(allTools);

	const comparable = (name) => name.toLowerCase().replace(/[^a-z0-9]/g, "");
	const toolNames = allTools.map((t) => t.name);
	const toolsProxy = new Proxy(tools, {
		get(object, property, receiver) {
			if (
				typeof property !== "string" ||
				property in object ||
				property in Object.prototype ||
				property === "then" ||
				property === "toJSON"
			) {
				return Reflect.get(object, property, receiver);
			}
			const wanted = comparable(property);
			const exact = toolNames.filter((name) => comparable(name) === wanted);
			const close =
				exact.length > 0
					? exact
					: toolNames.filter(
							(name) => wanted && (comparable(name).includes(wanted) || wanted.includes(comparable(name))),
						);
			let message = "tools." + property + " does not exist.";
			if (close.length > 0) {
				message += " Did you mean " + close.slice(0, 5).map((name) => "tools." + name).join(", ") + "?";
			} else if (toolNames.length <= 20 && toolNames.length > 0) {
				message += " Available: " + toolNames.join(", ") + ".";
			}
			message += " ALL_TOOLS lists every tool.";
			throw new TypeErrorCtor(message);
		},
	});

	// Synchronous JSON key-value store: key -> JSON text.
	const stored = new Map(Object.entries(parse(storeJson)));
	const writes = new Map();
	let storedChars = 0;
	for (const [key, json] of stored) storedChars += key.length + json.length;

	function checkKey(fnName, key) {
		if (typeof key !== "string") {
			throw new TypeErrorCtor(fnName + "() key must be a string");
		}
	}

	function store(key, value) {
		checkKey("store", key);
		const previous = stored.has(key) ? key.length + stored.get(key).length : 0;
		if (value === undefined) {
			stored.delete(key);
			storedChars -= previous;
			writes.set(key, undefined);
			return;
		}
		let json;
		try {
			json = stringify(value);
		} catch (error) {
			throw new TypeErrorCtor("store(" + stringify(key) + ") value is not JSON-serializable: " + format(error));
		}
		if (json === undefined) {
			throw new TypeErrorCtor("store(" + stringify(key) + ") value is not JSON-serializable");
		}
		if (json.length > __MAX_STORE_VALUE_CHARS__) {
			throw new RangeErrorCtor(
				"store(" +
					stringify(key) +
					") value has " +
					json.length +
					" characters of JSON, exceeding the limit of __MAX_STORE_VALUE_CHARS__",
			);
		}
		const next = storedChars - previous + key.length + json.length;
		if (next > __MAX_STORE_TOTAL_CHARS__) {
			throw new RangeErrorCtor(
				"store is full: stored values would exceed __MAX_STORE_TOTAL_CHARS__ characters of JSON. Delete keys with store(key, undefined).",
			);
		}
		stored.set(key, json);
		storedChars = next;
		writes.set(key, json);
	}

	function load(key) {
		checkKey("load", key);
		const json = stored.get(key);
		return json === undefined ? undefined : parse(json);
	}

	function serializeWrites() {
		const entries = [];
		for (const [key, json] of writes) {
			entries.push(json === undefined ? [key] : [key, json]);
		}
		return stringify(entries);
	}

	let outputChars = 0;
	let outputItems = 0;

	function output(kind, data, mimeType) {
		if (finished) return;
		outputChars += data.length;
		outputItems++;
		if (outputChars > __MAX_OUTPUT_CHARS__ || outputItems > __MAX_OUTPUT_ITEMS__) {
			const error = new RangeErrorCtor(
				"script output exceeded the limit of __MAX_OUTPUT_CHARS__ characters or __MAX_OUTPUT_ITEMS__ items",
			);
			done(false, describeError(error));
			throw error;
		}
		bridge("output", kind, data, mimeType);
	}

	function text(value) {
		let rendered;
		const t = typeof value;
		if (t === "string") {
			rendered = value;
		} else if (
			t === "number" ||
			t === "bigint" ||
			t === "boolean" ||
			t === "undefined" ||
			t === "symbol"
		) {
			rendered = String(value);
		} else {
			try {
				const json = stringify(value);
				rendered = json === undefined ? String(value) : json;
			} catch (error) {
				throw new TypeErrorCtor(error instanceof ErrorCtor ? error.message : String(error));
			}
		}
		output("text", rendered);
	}

	const IMAGE_EXPECTS =
		'image() expects a base64 data:image/<subtype>;base64,... URL, { image_url }, { type: "image", data, mimeType }, or { data, mime_type }';
	const IMAGE_MIME_RE = /^image\/[a-z0-9][a-z0-9.+-]*$/;
	const BASE64_RE = /^[A-Za-z0-9+/]+={0,2}$/;
	const B64_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

	function decodeBase64Prefix(data, maxBytes) {
		const out = [];
		const limit = Math.min(data.length, Math.ceil(maxBytes / 3) * 4);
		for (let i = 0; i < limit; i += 4) {
			const c0 = B64_ALPHABET.indexOf(data[i]);
			const c1 = B64_ALPHABET.indexOf(data[i + 1]);
			const p2 = data[i + 2] === "=";
			const p3 = data[i + 3] === "=";
			const c2 = p2 ? 0 : B64_ALPHABET.indexOf(data[i + 2]);
			const c3 = p3 ? 0 : B64_ALPHABET.indexOf(data[i + 3]);
			if (c0 < 0 || c1 < 0 || c2 < 0 || c3 < 0 || (p2 && !p3)) {
				return null;
			}
			const n = (c0 << 18) | (c1 << 12) | (c2 << 6) | c3;
			out.push((n >> 16) & 0xff);
			if (!p2) out.push((n >> 8) & 0xff);
			if (!p3) out.push(n & 0xff);
		}
		return out;
	}

	function detectImageMime(bytes) {
		if (!bytes || bytes.length < 3) return null;
		// PNG: 89 50 4E 47 0D 0A 1A 0A
		if (
			bytes.length >= 8 &&
			bytes[0] === 0x89 &&
			bytes[1] === 0x50 &&
			bytes[2] === 0x4e &&
			bytes[3] === 0x47 &&
			bytes[4] === 0x0d &&
			bytes[5] === 0x0a &&
			bytes[6] === 0x1a &&
			bytes[7] === 0x0a
		) {
			return "image/png";
		}
		// JPEG: FF D8 FF
		if (bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) {
			return "image/jpeg";
		}
		// GIF: GIF87a or GIF89a
		if (
			bytes.length >= 6 &&
			bytes[0] === 0x47 &&
			bytes[1] === 0x49 &&
			bytes[2] === 0x46 &&
			bytes[3] === 0x38 &&
			(bytes[4] === 0x37 || bytes[4] === 0x39) &&
			bytes[5] === 0x61
		) {
			return "image/gif";
		}
		// WebP: RIFF....WEBP
		if (
			bytes.length >= 12 &&
			bytes[0] === 0x52 &&
			bytes[1] === 0x49 &&
			bytes[2] === 0x46 &&
			bytes[3] === 0x46 &&
			bytes[8] === 0x57 &&
			bytes[9] === 0x45 &&
			bytes[10] === 0x42 &&
			bytes[11] === 0x50
		) {
			return "image/webp";
		}
		return null;
	}

	function rejectIfRemoteUrl(str) {
		const trimmed = str.trim().toLowerCase();
		if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
			throw new TypeErrorCtor(
				"remote image URLs (http:// and https://) are not supported; pass a base64 data URI or image object instead",
			);
		}
	}

	function parseDataImageUri(url) {
		rejectIfRemoteUrl(url);
		const trimmed = url.trim();
		const lower = trimmed.toLowerCase();
		if (!lower.startsWith("data:")) {
			throw new TypeErrorCtor("invalid image output: expected a base64 data:image/... URI");
		}
		const comma = trimmed.indexOf(",");
		if (comma === -1) {
			throw new TypeErrorCtor("invalid image output: malformed data URI");
		}
		const meta = trimmed.slice(5, comma).split(";").map((part) => part.trim().toLowerCase());
		const declaredMime = meta[0] || "";
		const isBase64 = meta.slice(1).includes("base64");
		if (!isBase64) {
			throw new TypeErrorCtor("invalid image output: data URI must be base64-encoded");
		}
		const rawData = trimmed.slice(comma + 1);
		return validateImageParts(rawData, declaredMime);
	}

	function validateImageParts(rawData, declaredMime) {
		if (typeof rawData !== "string") {
			throw new TypeErrorCtor("invalid image output: data must be a base64 string");
		}
		rejectIfRemoteUrl(rawData);
		const data = rawData.replace(/\s+/g, "");
		if (data.length === 0 || data.length % 4 !== 0 || !BASE64_RE.test(data)) {
			throw new TypeErrorCtor("invalid image output: image data is not valid base64");
		}
		const header = decodeBase64Prefix(data, 12);
		const detectedMime = detectImageMime(header);
		if (!detectedMime) {
			throw new TypeErrorCtor("invalid image output: unrecognized or unsupported image payload");
		}
		const declared = typeof declaredMime === "string" ? declaredMime.trim().toLowerCase() : "";
		if (declared !== "") {
			if (!IMAGE_MIME_RE.test(declared)) {
				throw new TypeErrorCtor('invalid image output: unsupported MIME type "' + declared + '"');
			}
			if (declared !== detectedMime) {
				throw new TypeErrorCtor(
					'invalid image output: declared MIME type "' +
						declared +
						'" does not match detected "' +
						detectedMime +
						'"',
				);
			}
		}
		return { data, mimeType: detectedMime };
	}

	function image(item) {
		let parsed;
		if (typeof item === "string") {
			if (item.trim() === "") throw new TypeErrorCtor(IMAGE_EXPECTS);
			parsed = parseDataImageUri(item);
		} else if (typeof item === "object" && item !== null && !Array.isArray(item)) {
			if (item.image_url !== undefined) {
				if (typeof item.image_url !== "string" || item.image_url.trim() === "") {
					throw new TypeErrorCtor(IMAGE_EXPECTS);
				}
				parsed = parseDataImageUri(item.image_url);
			} else {
				if (item.type !== undefined && item.type !== "image") {
					throw new TypeErrorCtor('image() only accepts image blocks, got "' + String(item.type) + '"');
				}
				if (typeof item.data !== "string" || item.data.trim() === "") {
					throw new TypeErrorCtor(IMAGE_EXPECTS);
				}
				rejectIfRemoteUrl(item.data);
				if (item.data.trim().toLowerCase().startsWith("data:")) {
					parsed = parseDataImageUri(item.data);
				} else {
					const mime = item.mimeType !== undefined ? item.mimeType : item.mime_type;
					if (mime !== undefined && typeof mime !== "string") {
						throw new TypeErrorCtor("invalid image output: mimeType/mime_type must be a string");
					}
					parsed = validateImageParts(item.data, mime || "");
				}
			}
		} else {
			throw new TypeErrorCtor(IMAGE_EXPECTS);
		}
		output("image", parsed.data, parsed.mimeType);
	}

	function exit() {
		let writesJson;
		try {
			writesJson = serializeWrites();
		} catch (error) {
			reportFailure(error);
			throw EXIT_SENTINEL;
		}
		done(true, undefined, writesJson);
		throw EXIT_SENTINEL;
	}

	const consoleObj = {};
	for (const level of ["log", "info", "warn", "error", "debug"]) {
		consoleObj[level] = (...args) => {
			output("console", args.map(format).join(" "));
		};
	}
	Object.freeze(consoleObj);

	Object.defineProperty(globalThis, "tools", { value: toolsProxy, enumerable: true });
	Object.defineProperty(globalThis, "ALL_TOOLS", { value: allTools, enumerable: true });
	Object.defineProperty(globalThis, "console", { value: consoleObj, enumerable: true });
	Object.defineProperty(globalThis, "text", { value: text, enumerable: true });
	Object.defineProperty(globalThis, "image", { value: image, enumerable: true });
	Object.defineProperty(globalThis, "exit", { value: exit, enumerable: true });
	Object.defineProperty(globalThis, "store", { value: store, enumerable: true });
	Object.defineProperty(globalThis, "load", { value: load, enumerable: true });

	return {
		settle(id, ok, payload) {
			const entry = pending.get(id);
			if (!entry) return;
			pending.delete(id);
			if (!ok) {
				entry.reject(new ErrorCtor(payload === undefined ? "Tool execution failed" : String(payload)));
				return;
			}
			let value;
			try {
				value = payload === undefined ? undefined : parse(payload);
			} catch (error) {
				entry.reject(error);
				return;
			}
			entry.resolve(value);
		},
		run(fn) {
			let promise;
			try {
				promise = fn(toolsProxy, consoleObj);
			} catch (error) {
				reportFailure(error);
				return;
			}
			promiseThen.call(
				promise,
				(value) => {
					let json;
					try {
						json = serialize(value);
					} catch (error) {
						reportFailure(error);
						return;
					}
					done(true, json, serializeWrites());
				},
				(error) => {
					reportFailure(error);
				},
			);
		},
		stalled() {
			if (finished || pending.size > 0) return false;
			finished = true;
			bridge("stalled");
			return true;
		},
	};
})"#;
