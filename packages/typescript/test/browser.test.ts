import assert from "node:assert/strict";
import test from "node:test";

import * as browser from "../src/browser.js";

test("browser entry exports the transport-neutral client only", () => {
  assert.equal(typeof browser.RuntimeClient.connect, "function");
  assert.equal("StdioGatewayTransport" in browser, false);
});
