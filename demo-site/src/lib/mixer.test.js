import { describe, expect, test } from "bun:test";

import { createMixer, gainOf, panOf, sampleAt } from "./mixer.js";

/**
 * A host that answers the `wh_audio_*` exports and nothing else.
 *
 * The sample buffer is deliberately the last thing in linear memory, so a mixer
 * that reads past the end of a voice's samples reads past the end of the
 * `Uint8Array` too and gets `undefined` -- which turns into a NaN in the mix.
 * That is the detector: a voice whose playhead runs away cannot be hidden.
 */
function fakeHost({ frames = 64, bits = 8, channels = 1, looping = true, frequency = 22_050 } = {}) {
  const bytesPerFrame = channels * (bits / 8);
  const sampleBytes = frames * bytesPerFrame;
  const eventsAt = 256;
  const buffer = new ArrayBuffer(eventsAt + 11 * 4 + sampleBytes);
  const memory = new Uint8Array(buffer);
  const samplesAt = eventsAt + 11 * 4;

  // A ramp, so every frame is distinguishable and none of them is zero.
  for (let index = 0; index < sampleBytes; index += 1) memory[samplesAt + index] = 40 + (index % 100);

  let pending = 0;
  let position = 0;
  const events = new Uint32Array(buffer, eventsAt, 11);

  return {
    memory: { buffer },
    wh_memory_ptr: () => 0,
    wh_audio_samples: (voice) => (voice === 1 ? samplesAt : 0),
    wh_audio_samples_len: (voice) => (voice === 1 ? sampleBytes : 0),
    wh_audio_pcm_ptr: () => 0,
    wh_audio_pcm_len: () => 0,
    wh_audio_position: () => position,
    wh_audio_event_count: () => pending,
    wh_audio_events_ptr: () => eventsAt,
    wh_audio_events_clear: () => {
      pending = 0;
    },
    /** Queues a play for voice 1. */
    play() {
      events.fill(0);
      events[0] = 0; // EVENT_PLAY
      events[1] = 1;
      events[2] = channels;
      events[3] = 22_050;
      events[4] = frames;
      events[7] = looping ? 1 : 0;
      events[8] = sampleBytes;
      events[9] = frequency;
      events[10] = bits;
      pending = 1;
    },
    /** Queues a stop for voice 1. */
    stop() {
      events.fill(0);
      events[0] = 2; // EVENT_STOP
      events[1] = 1;
      pending = 1;
    },
    setPosition(value) {
      position = value;
    },
  };
}

/** Mixes `blocks` blocks of `frames` and returns the interleaved peak. */
function mix(mixer, blocks, frames, rate = 48_000) {
  let peak = 0;
  let nan = 0;
  for (let index = 0; index < blocks; index += 1) {
    const left = new Float32Array(frames);
    const right = new Float32Array(frames);
    mixer.mixInto(left, right, rate);
    for (const value of left) {
      if (Number.isNaN(value)) nan += 1;
      else if (Math.abs(value) > peak) peak = Math.abs(value);
    }
  }
  return { peak, nan };
}

describe("a looping voice", () => {
  test("stays inside its buffer for far longer than the buffer holds", () => {
    // 64 frames of 8-bit mono is 2.9 ms of audio. Two seconds of it is 690
    // loops, and the playhead has to come back in range every one of them.
    const host = fakeHost({ frames: 64, bits: 8, looping: true });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    const { peak, nan } = mix(mixer, 160, 600);
    expect(nan).toBe(0);
    expect(peak).toBeGreaterThan(0);
  });

  test("is not removed by reaching the end of its buffer", () => {
    const host = fakeHost({ frames: 64, bits: 8, looping: true });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    mix(mixer, 40, 600);
    expect(mixer.size).toBe(1);
  });

  test("wraps instead of being subtracted from once", () => {
    // A playhead that moves further than the whole buffer in one output frame.
    // One subtraction leaves it out of range and the read walks off the end;
    // a modulo brings it back. 64 frames at 48 kHz is 0.75 source frames per
    // output frame, so the frequency has to be absurd for this to bite -- which
    // is exactly the case that produced full-scale noise instead of a note.
    const host = fakeHost({ frames: 64, bits: 8, looping: true, frequency: 4_294_967_295 });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    const { nan } = mix(mixer, 40, 600);
    expect(nan).toBe(0);
  });
});

describe("a one-shot voice", () => {
  test("is removed once it reaches the end", () => {
    const host = fakeHost({ frames: 64, bits: 8, looping: false });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    // 64 frames at 22.05 kHz is 2.9 ms, well inside one 12.5 ms block.
    mix(mixer, 4, 600);
    expect(mixer.size).toBe(0);
  });
});

describe("the event stream", () => {
  test("a stop takes the voice off the mixer", () => {
    const host = fakeHost({ frames: 64, bits: 8, looping: true });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    expect(mixer.size).toBe(1);
    host.stop();
    mixer.applyEvents();
    expect(mixer.size).toBe(0);
  });

  test("a play on a sounding voice retunes it rather than rewinding it", () => {
    const host = fakeHost({ frames: 64, bits: 8, looping: true });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    mix(mixer, 3, 600);
    host.play();
    mixer.applyEvents();
    expect(mixer.size).toBe(1);
    const { nan } = mix(mixer, 1, 600);
    expect(nan).toBe(0);
  });
});

describe("the mix", () => {
  test("never leaves full scale, however many voices are summed", () => {
    const host = fakeHost({ frames: 64, bits: 8, looping: true });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    const { peak } = mix(mixer, 20, 600);
    expect(peak).toBeLessThanOrEqual(1);
  });

  test("a silent voice table mixes silence and reports no peak", () => {
    const mixer = createMixer(fakeHost());
    const { peak, nan } = mix(mixer, 2, 600);
    expect(peak).toBe(0);
    expect(nan).toBe(0);
  });
});

describe("the playhead", () => {
  test("a fresh play starts at the guest's cursor, not at zero", () => {
    // `Play` on a stopped buffer resumes from the cursor, which can be anywhere
    // in the buffer. Slewing towards it would fade the start of the sound in
    // from the wrong place for several seconds.
    const host = fakeHost({ frames: 200, bits: 8, looping: false });
    const mixer = createMixer(host);
    host.setPosition(120);
    host.play();
    mixer.applyEvents();
    const left = new Float32Array(600);
    mixer.mixInto(left, new Float32Array(600), 48_000);
    // The fixture is a ramp of `40 + (index % 100)`; byte 120 reads 60, which
    // is (60 - 128) / 128 of full scale, halved by a centre pan.
    expect(left[0]).toBeCloseTo(((60 - 128) / 128) * Math.SQRT1_2, 3);
  });

  test("follows the guest's cursor when it moves somewhere else entirely", () => {
    // A seek is a real discontinuity in the guest's own playback, so the mixer
    // follows it rather than slewing towards it.
    const host = fakeHost({ frames: 64, bits: 8, looping: false });
    const mixer = createMixer(host);
    host.play();
    mixer.applyEvents();
    mix(mixer, 1, 600);
    host.setPosition(120);
    const { nan } = mix(mixer, 1, 600);
    expect(nan).toBe(0);
  });
});

describe("gainOf", () => {
  test("0 is unity, because that is DirectSound's default", () => {
    expect(gainOf(0)).toBe(1);
  });

  test("-10000 is -100 dB, which is silence to anyone listening", () => {
    // Hundredths of a decibel, so -10000 is 10^-5 and not zero: DirectSound
    // attenuates rather than mutes, and the difference is 100 dB down.
    expect(gainOf(-10_000)).toBeCloseTo(0.00001, 8);
  });

  test("-600 is about half", () => {
    expect(gainOf(-600)).toBeCloseTo(0.501, 2);
  });
});

describe("panOf", () => {
  test("clamps to the ends", () => {
    expect(panOf(-20_000)).toBe(-1);
    expect(panOf(20_000)).toBe(1);
    expect(panOf(0)).toBe(0);
  });
});

describe("sampleAt", () => {
  test("8-bit is unsigned and centred on 128", () => {
    const memory = new Uint8Array([128, 0, 255]);
    expect(sampleAt(memory, 0, 0, 1, 8)).toBe(0);
    expect(sampleAt(memory, 0, 1, 1, 8)).toBe(-1);
    expect(sampleAt(memory, 0, 2, 1, 8)).toBeCloseTo(0.992, 3);
  });

  test("16-bit is signed and little-endian", () => {
    const memory = new Uint8Array([0x00, 0x80, 0xff, 0x7f]);
    expect(sampleAt(memory, 0, 0, 2, 16)).toBe(-1);
    expect(sampleAt(memory, 0, 1, 2, 16)).toBeCloseTo(0.99997, 4);
  });
});
