import test from "node:test";
import assert from "node:assert/strict";
import { ticketEndpoint } from "../src/peer.js";

test("join-ticket endpoint substitutes URL-encoded discovery templates", () => {
  const endpoint = ticketEndpoint({
    origin: "https://provider.example",
    endpoints: { join_tickets: "https://provider.example/v1/projects/%7Bproject_id%7D/join-tickets" }
  }, "prj_example");
  assert.equal(endpoint, "https://provider.example/v1/projects/prj_example/join-tickets");
});
