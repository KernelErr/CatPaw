// Evaluated once per realm before any page script. Defines the pieces of the
// platform that are simplest to express in JavaScript and hands the host the
// functions it needs to keep (so that page scripts replacing globals such as
// JSON cannot interfere).
(function (global) {
  "use strict";

  const legacyCodes = {
    IndexSizeError: 1,
    HierarchyRequestError: 3,
    WrongDocumentError: 4,
    InvalidCharacterError: 5,
    NoModificationAllowedError: 7,
    NotFoundError: 8,
    NotSupportedError: 9,
    InUseAttributeError: 10,
    InvalidStateError: 11,
    SyntaxError: 12,
    InvalidModificationError: 13,
    NamespaceError: 14,
    InvalidAccessError: 15,
    TypeMismatchError: 17,
    SecurityError: 18,
    NetworkError: 19,
    AbortError: 20,
    URLMismatchError: 21,
    QuotaExceededError: 22,
    TimeoutError: 23,
    InvalidNodeTypeError: 24,
    DataCloneError: 25,
  };

  const exceptionState = new WeakMap();
  const stateOf = (object) => {
    const state = exceptionState.get(object);
    if (!state) throw new TypeError("Illegal invocation");
    return state;
  };

  class DOMException extends Error {
    constructor(message = "", name = "Error") {
      super();
      exceptionState.set(this, { message: String(message), name: String(name) });
    }
    get name() {
      return stateOf(this).name;
    }
    get message() {
      return stateOf(this).message;
    }
    get code() {
      return legacyCodes[stateOf(this).name] || 0;
    }
  }

  const constants = {
    INDEX_SIZE_ERR: 1,
    DOMSTRING_SIZE_ERR: 2,
    HIERARCHY_REQUEST_ERR: 3,
    WRONG_DOCUMENT_ERR: 4,
    INVALID_CHARACTER_ERR: 5,
    NO_DATA_ALLOWED_ERR: 6,
    NO_MODIFICATION_ALLOWED_ERR: 7,
    NOT_FOUND_ERR: 8,
    NOT_SUPPORTED_ERR: 9,
    INUSE_ATTRIBUTE_ERR: 10,
    INVALID_STATE_ERR: 11,
    SYNTAX_ERR: 12,
    INVALID_MODIFICATION_ERR: 13,
    NAMESPACE_ERR: 14,
    INVALID_ACCESS_ERR: 15,
    VALIDATION_ERR: 16,
    TYPE_MISMATCH_ERR: 17,
    SECURITY_ERR: 18,
    NETWORK_ERR: 19,
    ABORT_ERR: 20,
    URL_MISMATCH_ERR: 21,
    QUOTA_EXCEEDED_ERR: 22,
    TIMEOUT_ERR: 23,
    INVALID_NODE_TYPE_ERR: 24,
    DATA_CLONE_ERR: 25,
  };
  for (const [key, value] of Object.entries(constants)) {
    const descriptor = { value, enumerable: true, writable: false, configurable: false };
    Object.defineProperty(DOMException, key, descriptor);
    Object.defineProperty(DOMException.prototype, key, descriptor);
  }
  for (const key of ["name", "message", "code"]) {
    const descriptor = Object.getOwnPropertyDescriptor(DOMException.prototype, key);
    descriptor.enumerable = true;
    Object.defineProperty(DOMException.prototype, key, descriptor);
  }
  Object.defineProperty(DOMException.prototype, Symbol.toStringTag, {
    value: "DOMException",
    configurable: true,
  });
  Object.defineProperty(global, "DOMException", {
    value: DOMException,
    writable: true,
    configurable: true,
    enumerable: false,
  });

  const objectToString = Object.prototype.toString;
  const errorConstructors = { EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError };

  // The structured clone algorithm for values that stay in this realm.
  function structuredClone(value) {
    const seen = new Map();
    const fail = (what) => {
      throw new DOMException(`${what} could not be cloned.`, "DataCloneError");
    };
    const clone = (input) => {
      const type = typeof input;
      if (type === "function") fail("A function");
      if (type === "symbol") fail("A symbol");
      if (input === null || type !== "object") return input;
      if (seen.has(input)) return seen.get(input);

      const tag = objectToString.call(input).slice(8, -1);
      const remember = (output) => {
        seen.set(input, output);
        return output;
      };
      switch (tag) {
        case "Boolean":
        case "Number":
        case "String":
        case "BigInt":
          return remember(Object(input.valueOf()));
        case "Date":
          return remember(new Date(input.getTime()));
        case "RegExp":
          return remember(new RegExp(input.source, input.flags));
        case "ArrayBuffer":
          return remember(input.slice(0));
        case "Map": {
          const output = remember(new Map());
          for (const [key, item] of input) output.set(clone(key), clone(item));
          return output;
        }
        case "Set": {
          const output = remember(new Set());
          for (const item of input) output.add(clone(item));
          return output;
        }
        case "Array": {
          const output = remember(new Array(input.length));
          for (const key of Object.keys(input)) output[key] = clone(input[key]);
          return output;
        }
        case "Error": {
          const constructor = errorConstructors[input.name] || Error;
          const output = remember(new constructor(input.message));
          if (typeof input.stack === "string") {
            Object.defineProperty(output, "stack", {
              value: input.stack,
              writable: true,
              configurable: true,
            });
          }
          return output;
        }
        case "Object": {
          const output = remember({});
          for (const key of Object.keys(input)) output[key] = clone(input[key]);
          return output;
        }
        default:
          if (ArrayBuffer.isView(input)) {
            const buffer = clone(input.buffer);
            return remember(
              tag === "DataView"
                ? new DataView(buffer, input.byteOffset, input.byteLength)
                : new input.constructor(buffer, input.byteOffset, input.length),
            );
          }
          return fail(`A ${tag} object`);
      }
    };
    return clone(value);
  }

  return {
    DOMException,
    structuredClone,
    jsonParse: JSON.parse,
    jsonStringify: JSON.stringify,
  };
})(globalThis);
