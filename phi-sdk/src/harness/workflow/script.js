const { agent, parallel, pipeline, phase, log, flush } = ((host, limits) => {
	const operational = new WeakSet();
	let currentPhase;
	let progress = Promise.resolve();
	let outputBytes = 0;
	let observationCount = 0;

	async function call(request) {
		const response = await host(request);
		if (response.kind === "success") return response.value;
		const error = new Error(response.message);
		if (response.kind === "operational") operational.add(error);
		throw error;
	}

	function agent(prompt, options = {}) {
		if (typeof prompt !== "string" || !prompt.trim()) throw new TypeError("agent prompt must be a nonempty string");
		if (typeof options !== "object" || options === null || Array.isArray(options)) throw new TypeError("agent options must be an object");
		if (Reflect.ownKeys(options).some(key => key !== "role" && key !== "label")) throw new TypeError("unsupported agent option");
		return call({ operation: "agent", prompt, options, phase: currentPhase });
	}

	function observe(request, text) {
		for (const character of text) {
			const code = character.codePointAt(0);
			outputBytes += code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
		}
		observationCount++;
		if (outputBytes > limits.outputBytes || observationCount > limits.observations) throw new RangeError("workflow output limit reached");
		progress = progress.then(() => call(request));
	}

	function phase(title) {
		if (typeof title !== "string" || !title.trim()) throw new TypeError("phase title must be a nonempty string");
		observe({ operation: "phase", title }, title);
		currentPhase = title;
	}

	function log(value) {
		const message = typeof value === "string" ? value : value === undefined ? "undefined" : JSON.stringify(value);
		if (typeof message !== "string") throw new TypeError("log value must be JSON serializable");
		observe({ operation: "log", message }, message);
	}

	async function compose(items, callback) {
		if (!Array.isArray(items) || items.length > limits.items) throw new TypeError("composition requires a bounded array");
		for (let index = 0; index < items.length; index++) {
			if (!Object.hasOwn(items, index)) throw new TypeError("composition requires a dense array");
		}
		const results = new Array(items.length);
		let next = 0;
		let failure;
		let failed = false;
		async function consume() {
			while (!failed && next < items.length) {
				const index = next++;
				try {
					results[index] = await callback(items[index], index);
				} catch (error) {
					if (operational.has(error)) results[index] = null;
					else { failure = error; failed = true; }
				}
			}
		}
		await Promise.all(Array.from({ length: Math.min(items.length, limits.concurrency) }, consume));
		if (failed) throw failure;
		return results;
	}

	function parallel(functions) {
		if (!Array.isArray(functions)) throw new TypeError("parallel requires deferred functions");
		for (const callback of functions) {
			if (typeof callback !== "function") throw new TypeError("parallel requires deferred functions");
		}
		return compose(functions, callback => callback());
	}

	function pipeline(items, ...stages) {
		if (stages.length === 0 || stages.some(stage => typeof stage !== "function")) throw new TypeError("pipeline requires stage functions");
		return compose(items, async (item, index) => {
			for (const stage of stages) item = await stage(item, index);
			return item;
		});
	}

	return { agent, parallel, pipeline, phase, log, flush: () => progress };
})(workflowHost, WORKFLOW_LIMITS);
