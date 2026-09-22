// Integration test against the *real* `@serenity-kit/opaque` WASM module
// (unlike `hostAuth.test.ts`, which only exercises the pure label/phase
// functions) -- confirms `opaqueClient` (this module's one real
// `OpaqueClient`) actually interops with an OPAQUE server run the same way
// `host/src/access.rs` runs one: same cipher suite (the package always uses
// it), same `keyStretching: "memory-constrained"`, same fixed
// `userIdentifier`/`CREDENTIAL_ID` of `"rcdesk-owner"`, no `identifiers`.
// The server side here is plain `@serenity-kit/opaque` calls (not a browser,
// not the Rust host) standing in for the host's registration/login --
// exactly the JS API surface `host/src/access.rs`'s doc comment says the
// Rust side mirrors byte-for-byte. Slow (~1-2s: two Argon2id "memory-
// constrained" runs, one per client-side finish call) -- expected, not a
// bug; see slice 3.2d's prompt.

import { describe, expect, it } from "vitest";
import * as opaque from "@serenity-kit/opaque";
import { opaqueClient } from "./hostAuth";

const USER_IDENTIFIER = "rcdesk-owner";

/** Runs a full OPAQUE registration for `password` entirely in JS (the
 * server side stands in for `host::access::register`) and returns the
 * resulting `registrationRecord`, ready to feed into `server.startLogin`. */
async function registerPassword(
  serverSetup: string,
  password: string,
): Promise<string> {
  await opaque.ready;
  const { clientRegistrationState, registrationRequest } = opaque.client.startRegistration({
    password,
  });
  const { registrationResponse } = opaque.server.createRegistrationResponse({
    serverSetup,
    userIdentifier: USER_IDENTIFIER,
    registrationRequest,
  });
  const { registrationRecord } = opaque.client.finishRegistration({
    password,
    registrationResponse,
    clientRegistrationState,
    keyStretching: "memory-constrained",
  });
  return registrationRecord;
}

describe("opaqueClient against a real OPAQUE server", () => {
  it("derives the same session key as the server for the correct password", async () => {
    await opaque.ready;
    const password = "correct horse battery staple";
    const serverSetup = opaque.server.createSetup();
    const registrationRecord = await registerPassword(serverSetup, password);

    const { state, request } = await opaqueClient.startLogin(password);
    const { serverLoginState, loginResponse } = opaque.server.startLogin({
      serverSetup,
      registrationRecord,
      startLoginRequest: request,
      userIdentifier: USER_IDENTIFIER,
    });

    const result = await opaqueClient.finishLogin(state, loginResponse, password);
    expect(result).not.toBeNull();
    const { finalization, sessionKey: clientSessionKey } = result!;

    const { sessionKey: serverSessionKey } = opaque.server.finishLogin({
      serverLoginState,
      finishLoginRequest: finalization,
    });

    expect(clientSessionKey).toBe(serverSessionKey);
  }, 20_000);

  it("returns null from finishLogin for the wrong password", async () => {
    await opaque.ready;
    const serverSetup = opaque.server.createSetup();
    const registrationRecord = await registerPassword(serverSetup, "correct horse battery staple");

    const { state, request } = await opaqueClient.startLogin("wrong password");
    const { loginResponse } = opaque.server.startLogin({
      serverSetup,
      registrationRecord,
      startLoginRequest: request,
      userIdentifier: USER_IDENTIFIER,
    });

    const result = await opaqueClient.finishLogin(state, loginResponse, "wrong password");
    expect(result).toBeNull();
  }, 20_000);
});
