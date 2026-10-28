import { describe, expect, test } from "bun:test";

import { keptBy, leafName } from "./player.js";

describe("leafName", () => {
  test("keeps a bare name and lowercases it", () => {
    expect(leafName("Save.DAT")).toBe("save.dat");
  });

  test("takes the last segment of either separator", () => {
    expect(leafName("Game.exe\\save.dat")).toBe("save.dat");
    expect(leafName("data/slot1.dat")).toBe("slot1.dat");
    expect(leafName("C:\\Games\\Game\\save.dat")).toBe("save.dat");
  });
});

describe("keptBy", () => {
  const saves = ["save.dat", "record.dat"];

  test("a payload with no list keeps every write", () => {
    expect(keptBy(null, "Game.exe\\anything.dat")).toBe(true);
    expect(keptBy([], "anything.dat")).toBe(true);
  });

  test("a bare guest name matches a listed one, in any case", () => {
    expect(keptBy(saves, "SAVE.DAT")).toBe(true);
  });

  test("a path the guest built still matches its leaf", () => {
    expect(keptBy(saves, "Game.exe\\save.dat")).toBe(true);
    expect(keptBy(saves, "C:\\Games\\Game\\record.dat")).toBe(true);
    expect(keptBy(saves, "data/save.dat")).toBe(true);
  });

  test("a listing that names a folder is compared leaf to leaf", () => {
    expect(keptBy(["data\\save.dat"], "Game.exe\\save.dat")).toBe(true);
  });

  test("a file the payload does not list is dropped", () => {
    expect(keptBy(saves, "Game.exe\\Config.dat")).toBe(false);
  });

  test("`*` keeps everything", () => {
    expect(keptBy(["*"], "Game.exe\\Config.dat")).toBe(true);
  });
});
