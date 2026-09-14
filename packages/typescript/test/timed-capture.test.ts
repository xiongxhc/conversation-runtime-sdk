import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { parseClientCommand,parseGatewayMessage } from "../src/protocol.js";

test("frozen timed start and page credit reach explicit v4 admission only", async () => {
  const fixture = JSON.parse(await readFile(new URL("../../../../tests/fixtures/timed-capture-v1/control.json", import.meta.url), "utf8"));
  for (const wire of [fixture.start, fixture.credit, fixture.cancel]) {
    assert.equal(parseClientCommand(wire, 4).type, wire.type);
    for (const protocol_version of [1, 2, 3] as const) {
      assert.throws(() => parseClientCommand({ ...wire, protocol_version }, protocol_version));
    }
  }
});

test("canonical timed results preserve exact nominal coordinates and nullable physical accuracy", async (t) => {
  const fixture=JSON.parse(await readFile(new URL("../../../../tests/fixtures/timed-capture-v1/output.json",import.meta.url),"utf8"));
  t.mock.method(Date,"now",()=>fixture.fixtureClockUnixMs);
  const started=parseGatewayMessage(fixture.started,4);assert.equal(started.type,"timed_voice_session_started");
  if(started.type!=="timed_voice_session_started") assert.fail();
  assert.equal(started.mediaLease.expiresAtMs,1060000n);assert.equal(started.mediaLease.mediaProtocolVersion,2);assert.equal(started.mediaLease.captureFormat,"pcm_s16le/24000/1/20ms");
  const parsed=parseGatewayMessage(fixture.page,4);assert.equal(parsed.type,"recording_alignment_page");
  if(parsed.type!=="recording_alignment_page") assert.fail();
  const frame=parsed.page.entries[1];assert.equal(frame?.kind,"frame");if(frame?.kind!=="frame") assert.fail();
  assert.equal(frame.provenance?.quality,"nominal_frame_endpoint_interpolation");assert.equal(frame.provenance?.sourceEndNumerator,96000n);
  assert.equal(frame.wall?.localStartNs,2100000000n);assert.equal(frame.wall?.overallPhysicalAccuracyNs,null);assert.equal(frame.wall?.captureUncertaintyNs,null);
  assert.equal(parseGatewayMessage(fixture.complete,4).type,"recording_alignment_complete");
  for(const version of [1,2,3] as const) for(const wire of [fixture.started,fixture.page,fixture.complete]) assert.throws(()=>parseGatewayMessage({...wire,protocol_version:version},version));
  for(const mutate of [
    (page:any)=>page.entries.push(...Array(64).fill(page.entries[0])),
    (page:any)=>page.entries[1].provenance.span.capture_uncertainty_ns="18446744073709551616",
    (page:any)=>page.entries[1].wall.overall_physical_accuracy_ns="0",
    (page:any)=>page.entries[1].provenance.source_end_numerator="96001",
    (page:any)=>page.entries[1].provenance.quality="piecewiseNominalSourceInterpolation",
    (page:any)=>page.entries[1].secret="must-not-appear-in-error",
  ]) {
    const wire=structuredClone(fixture.page);mutate(wire.page);
    assert.throws(()=>parseGatewayMessage(wire,4),(error:unknown)=>error instanceof Error&&!error.message.includes("must-not-appear"));
  }
});
