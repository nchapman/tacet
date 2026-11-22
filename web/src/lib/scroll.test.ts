import { describe, expect, it } from "vitest";
import { ScrollStore } from "./scroll";

describe("ScrollStore", () => {
  it("stores, rounds, clamps, and evicts least-recently-used scroll positions", () => {
    const store = new ScrollStore(2);
    store.save("a", 412.6);
    store.save("b", -15);
    expect(store.get("a")).toBe(413);
    expect(store.get("b")).toBe(0);

    // Access 'a' again so 'b' is evicted when 'c' is added.
    store.get("a");
    store.save("c", 900);

    expect(store.get("a")).toBe(413);
    expect(store.get("b")).toBeUndefined();
    expect(store.get("c")).toBe(900);
    expect(store.size()).toBe(2);
  });
});
