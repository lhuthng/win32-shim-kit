// The DirectSound mixer.
//
// A game hands DirectSound a dozen small waveforms at once and DirectSound adds
// them up; this is that mixer. It is fed by nothing but the host's event stream,
// and it reads each voice's samples where the guest keeps them, so a buffer the
// guest rewrites under a playing voice is heard as it is.
//
// It lives in its own module, apart from the player, so it can be driven without
// a browser: `tmp/hercules-probe/mix-lab.mjs` boots a payload, feeds this the
// real event stream and writes the mix out as a WAV. A mixer nobody can listen
// to offline is a mixer whose faults can only be guessed at, and guessing is
// what produced the three wrong answers that preceded this file.

/** One event is eleven 32-bit words; see `AudioEvent` in the host. */
export const EVENT_WORDS = 11;
export const EVENT_PLAY = 0;
export const EVENT_UPDATE = 1;
export const EVENT_STOP = 2;
export const EVENT_RELEASE = 3;
export const EVENT_DATA = 4;
/** The guest moved the playhead with `SetCurrentPosition`; its offset is `bytes`. */
export const EVENT_SEEK = 5;

/**
 * How far a voice may be retuned, as a multiple of the mix rate.
 *
 * DirectSound accepts 100 Hz to 100 kHz, and a guest that retunes a waveform is
 * expected to stay inside that. This is not politeness: `step` is a distance the
 * playhead moves per output frame, and a guest that passes a nonsense frequency
 * -- a byte count, a negative number widened to `u32` -- would otherwise move
 * the playhead past the end of the buffer on the first frame. The read then
 * walks off into whatever follows it in guest memory, which is heard as full
 * scale noise, and a looping voice in that state never comes back in range, so
 * it never ends. Both of the reported symptoms, from one unclamped number.
 */
const MAX_RATIO = 64;
const MIN_RATIO = 1 / 64;

/**
 * How much of a block's playhead movement may be spent catching up to the
 * guest's play cursor.
 *
 * The playhead belongs to this mixer. The guest's cursor is a *reference*, and
 * the difference between the two is closed by nudging the rate by this fraction
 * per block -- never by moving the playhead. A move is a step in the waveform,
 * and a step twenty times a second is the rasp that was reported as distortion.
 */
const MAX_SLEW = 0.005;

/**
 * Beyond this much error the playhead is re-seeded rather than slewed.
 *
 * Slewing closes a small gap; it cannot close a seek, a resume from `Stop`, or
 * the gap a stalled tab leaves. Those are real discontinuities in the guest's
 * own playback, and following them is correct.
 */
const SNAP_SECONDS = 0.25;

/**
 * Errors smaller than this are left alone.
 *
 * The guest's play cursor is whole milliseconds -- `GetCurrentPosition` answers
 * in bytes derived from an integer millisecond clock -- so it arrives in steps
 * of about 22 frames on a 22 kHz buffer while this playhead moves smoothly.
 * Steering towards it every block would make the rate wobble at the block rate,
 * which is a growl, not a correction. Below the width of that quantisation
 * there is nothing to correct, and the playhead is the better clock.
 */
const SLEW_DEADBAND = 0.002;

/** Where the soft clip starts. Below this the mix is untouched. */
const CLIP_KNEE = 0.9;

/** DirectSound volume is hundredths of a decibel: 0 is unity, -10000 silent. */
export function gainOf(volume) {
  if (!volume) return 1;
  return Math.min(1, Math.pow(10, volume / 2000));
}

/** DirectSound pan is -10000 (left) to 10000 (right). */
export function panOf(pan) {
  return Math.max(-1, Math.min(1, pan / 10000));
}

/** One frame of a voice's buffer, as a fraction of full scale. */
export function sampleAt(memory, base, index, bytes, bits) {
  const at = base + index * bytes;
  if (bits === 8) return (memory[at] - 128) / 128;
  // 16-bit samples are signed and little-endian.
  return (((memory[at] | (memory[at + 1] << 8)) << 16) >> 16) / 32768;
}

/**
 * A mix that has run past full scale, folded back without a time constant.
 *
 * DirectSound summed its voices in integers and clipped at full scale; it never
 * compressed. A `DynamicsCompressorNode` stood here as a "safety limiter" and it
 * was itself a source of the distortion: at a -1 dB threshold, a 20:1 ratio and
 * a 250 ms release it is engaged for most of a busy mix, so it squashed the
 * music and breathed with every drum hit. This is exactly linear below
 * `CLIP_KNEE` and saturates smoothly above it, so it touches only what would
 * have clipped anyway and has no release to pump with.
 */
function softClip(value) {
  const magnitude = value < 0 ? -value : value;
  if (magnitude <= CLIP_KNEE) return value;
  const over = (magnitude - CLIP_KNEE) / (1 - CLIP_KNEE);
  const shaped = CLIP_KNEE + (1 - CLIP_KNEE) * Math.tanh(over);
  return value < 0 ? -shaped : shaped;
}

/**
 * The voice table, and the mix.
 *
 * `host` is the wasm module: the mixer reads its memory directly and asks it for
 * a voice's samples, length and play cursor through the `wh_audio_*` exports.
 */
export function createMixer(host) {
  /**
   * One entry per sounding voice: its format, and where the playhead has got
   * to. The samples are not copied -- the mixer reads them where the guest
   * keeps them.
   *
   * `position` is in *source frames* and is a float. It is this mixer's own
   * playhead, advanced by `step` for every output frame rendered, and it is the
   * only thing that decides which sample is read next.
   */
  const voices = new Map();

  /** Source frames this voice has rendered, for the diagnostic. */
  const ages = new Map();

  function play(voice, spec) {
    const sounding = voices.get(voice);
    if (sounding) {
      // `Play` starts at the play cursor, so a voice that is already sounding
      // keeps playing: this is a sustained looping waveform being retuned, not
      // a new note. Rewinding here would cut the wave at every note.
      sounding.spec = { ...sounding.spec, ...spec };
      return;
    }
    // A voice that has just started takes its position from the guest's cursor
    // on the first block rather than slewing towards it: a `Play` that resumes
    // from the middle of a buffer is not a drift to be corrected, it is where
    // the sound begins.
    voices.set(voice, { spec, position: 0, fresh: true });
    ages.set(voice, 0);
  }

  /**
   * Applies every queued event to the voice table.
   *
   * Returns how many were read, so a caller can tell an empty queue from a
   * queue it forgot to drain.
   */
  function applyEvents() {
    const count = host.wh_audio_event_count();
    if (!count) return 0;
    const base = host.wh_audio_events_ptr() >>> 0;
    const memory = new Uint8Array(host.memory.buffer);
    const words = new Uint32Array(memory.buffer, base, count * EVENT_WORDS);
    for (let index = 0; index < count; index += 1) {
      const at = index * EVENT_WORDS;
      const kind = words[at];
      const voice = words[at + 1];
      if (kind === EVENT_PLAY) {
        play(voice, {
          channels: words[at + 2],
          rate: words[at + 3],
          volume: words[at + 5] | 0,
          pan: words[at + 6] | 0,
          looping: (words[at + 7] & 1) === 1,
          frequency: words[at + 9],
          bits: words[at + 10],
        });
      } else if (kind === EVENT_STOP) {
        // A stopped buffer keeps its play cursor in the host, which is where a
        // later `Play` resumes from, so this mixer's entry can go. Keeping every
        // stopped voice is how a frame's worth of sounds ends up mixed together.
        voices.delete(voice);
        ages.delete(voice);
      } else if (kind === EVENT_RELEASE) {
        voices.delete(voice);
        ages.delete(voice);
      } else if (kind === EVENT_SEEK) {
        // The guest moved the playhead itself, which is how a drum restarts.
        const entry = voices.get(voice);
        if (entry) {
          const bytes = entry.spec.channels * (entry.spec.bits / 8);
          entry.position = bytes ? words[at + 8] / bytes : 0;
        }
      } else if (kind === EVENT_UPDATE) {
        const entry = voices.get(voice);
        if (entry) {
          // An update carries the frequency and not the buffer's own rate --
          // `SetFrequency` is how a game retunes a waveform it is already
          // playing, which is every note of a pitched instrument.
          entry.spec = {
            ...entry.spec,
            volume: words[at + 5] | 0,
            pan: words[at + 6] | 0,
            frequency: words[at + 9] || entry.spec.frequency,
          };
        }
      }
      // `EVENT_DATA` needs nothing here: the mixer reads the buffer as it
      // stands, so a write the guest makes is heard on the next block.
    }
    host.wh_audio_events_clear();
    return count;
  }

  /** Where a voice's samples are in linear memory, and how many bytes. */
  function locate(voice) {
    let base = 0;
    let length = 0;
    // Live memory first: a streaming guest writes into the buffer it plays.
    const address = host.wh_audio_samples ? host.wh_audio_samples(voice) >>> 0 : 0;
    if (address) {
      length = host.wh_audio_samples_len(voice) >>> 0;
      base = (host.wh_memory_ptr() >>> 0) + address;
    }
    if (!length) {
      // A voice with no live buffer: the host kept the copy it took.
      length = host.wh_audio_pcm_len ? host.wh_audio_pcm_len(voice) >>> 0 : 0;
      base = host.wh_audio_pcm_ptr ? host.wh_audio_pcm_ptr(voice) >>> 0 : 0;
    }
    return { base, length };
  }

  /**
   * Renders every voice into `left` and `right`, at `rate`.
   *
   * Returns the peak of the mix before the soft clip, so a caller can see how
   * far past full scale it went and whether the clip is doing anything at all.
   */
  function mixInto(left, right, rate) {
    const frames = left.length;
    if (!frames) return { peak: 0, clipped: 0 };
    const memory = new Uint8Array(host.memory.buffer);
    let peak = 0;

    for (const [voice, entry] of voices) {
      const spec = entry.spec;
      const bytes = spec.channels * (spec.bits / 8);
      if (!spec.channels || !spec.bits || !spec.rate || !bytes) continue;

      const { base, length } = locate(voice);
      const available = length ? Math.floor(length / bytes) : 0;
      if (!available) {
        // Nothing to read: the guest's buffer is gone and the host kept no copy.
        voices.delete(voice);
        ages.delete(voice);
        continue;
      }

      // `frequency` is the playback rate in Hz against the buffer's own rate,
      // which is what makes DirectSound's tiny looping waveforms notes. One
      // source frame therefore lasts `frequency / rate` output frames.
      let step = (spec.frequency || spec.rate) / rate;
      if (!Number.isFinite(step) || step <= 0) step = spec.rate / rate;
      step = Math.min(MAX_RATIO, Math.max(MIN_RATIO, step));

      let position = entry.position;
      if (!Number.isFinite(position)) position = 0;
      if (spec.looping) position = ((position % available) + available) % available;

      // Steer the playhead towards the guest's cursor by rate, never by jumps.
      const cursor = host.wh_audio_position ? host.wh_audio_position(voice) >>> 0 : 0;
      if (cursor) {
        const target = cursor / bytes;
        if (entry.fresh) {
          // Where the sound begins, not a drift to correct.
          entry.fresh = false;
          position = spec.looping ? ((target % available) + available) % available : target;
        } else {
          let error = target - position;
          if (spec.looping) {
            // The shortest way round the loop, so a cursor just past the seam
            // does not read as an error of nearly a whole buffer.
            if (error > available / 2) error -= available;
            if (error < -available / 2) error += available;
          }
          if (Math.abs(error) > SNAP_SECONDS * spec.rate) {
            // A seek, a resume, a stall: a real discontinuity in the guest's own
            // playback. Follow it.
            position = spec.looping ? ((target % available) + available) % available : target;
          } else if (Math.abs(error) > SLEW_DEADBAND * spec.rate) {
            const nominal = frames * step;
            if (nominal > 0) {
              const adjust = error / nominal;
              step *= 1 + Math.max(-MAX_SLEW, Math.min(MAX_SLEW, adjust));
            }
          }
        }
      }

      const gain = gainOf(spec.volume);
      const angle = ((panOf(spec.pan) + 1) / 2) * (Math.PI / 2);
      const leftGain = Math.cos(angle) * gain;
      const rightGain = Math.sin(angle) * gain;

      // A looping buffer wraps; a one-shot stops. The read wraps the same way,
      // so the interpolation across the seam uses the buffer's first sample and
      // not the end of whatever is next in memory.
      const read = (index) => {
        let at = index;
        if (spec.looping) {
          at %= available;
          if (at < 0) at += available;
        } else if (at < 0) {
          at = 0;
        } else if (at >= available) {
          at = available - 1;
        }
        return sampleAt(memory, base, at, bytes, spec.bits);
      };

      for (let index = 0; index < frames; index += 1) {
        if (position >= available) {
          if (!spec.looping) break;
          // A modulo, not a subtraction: `step` may be larger than the buffer,
          // and one subtraction would leave the playhead out of range and the
          // read in whatever memory follows.
          position %= available;
        }
        if (position < 0) position = 0;
        const frame = Math.floor(position);
        const between = position - frame;
        // Catmull-Rom across four taps. Linear interpolation droops by nearly
        // 8 dB at the top of a 22 kHz buffer, which is heard as a dull mix;
        // this costs three more reads and is flat to within a decibel.
        const s0 = read(frame - 1);
        const s1 = read(frame);
        const s2 = read(frame + 1);
        const s3 = read(frame + 2);
        const value =
          s1 +
          0.5 *
            between *
            (s2 -
              s0 +
              between *
                (2 * s0 - 5 * s1 + 4 * s2 - s3 + between * (3 * (s1 - s2) + s3 - s0)));
        left[index] += value * leftGain;
        right[index] += value * rightGain;
        position += step;
      }

      entry.position = position;
      ages.set(voice, (ages.get(voice) ?? 0) + frames * step);
      if (!spec.looping && position >= available) {
        // A one-shot that has reached its end is finished. Keeping it holds a
        // voice for every sound the game has ever played.
        voices.delete(voice);
        ages.delete(voice);
      }
    }

    let clipped = 0;
    for (let index = 0; index < frames; index += 1) {
      const l = left[index];
      const r = right[index];
      const level = (l < 0 ? -l : l) > (r < 0 ? -r : r) ? (l < 0 ? -l : l) : (r < 0 ? -r : r);
      if (level > peak) peak = level;
      if (level > CLIP_KNEE) clipped += 1;
      left[index] = softClip(l);
      right[index] = softClip(r);
    }
    return { peak, clipped };
  }

  /**
   * What the voice table holds, for the page's diagnostic line.
   *
   * `longest` is the age of the oldest looping voice in seconds of its own
   * audio. A looping voice has no end of its own, so only a Stop or a Release
   * can take it off the mixer: an age of minutes on a one-second buffer is a
   * sound that will not go off, named without anyone having to listen for it.
   */
  function stats() {
    let looping = 0;
    let longest = 0;
    let voice = 0;
    for (const [id, entry] of voices) {
      if (!entry.spec.looping) continue;
      looping += 1;
      const rate = entry.spec.frequency || entry.spec.rate || 1;
      const age = (ages.get(id) ?? 0) / rate;
      if (age > longest) {
        longest = age;
        voice = id;
      }
    }
    return { count: voices.size, looping, longest, voice };
  }

  return {
    applyEvents,
    mixInto,
    stats,
    clear() {
      voices.clear();
      ages.clear();
    },
    get size() {
      return voices.size;
    },
  };
}
