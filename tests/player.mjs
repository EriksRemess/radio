import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { setImmediate } from 'node:timers/promises';
import { runInNewContext } from 'node:vm';

const script = readFileSync(new URL('../src/player.html', import.meta.url), 'utf8')
  .match(/<script>([\s\S]*?)<\/script>/)[1];

function player() {
  const elements = new Map();
  function element(id) {
    if (!elements.has(id)) {
      const node = new EventTarget();
      Object.assign(node, {
        dataset: {}, options: [], attributes: {},
        setAttribute(name, value) { this.attributes[name] = value; },
        removeAttribute(name) { delete this[name]; },
      });
      elements.set(id, node);
    }
    return elements.get(id);
  }
  const pending = [];
  const audio = element('#audio');
  Object.assign(audio, {
    volume: 1, muted: false, load() {}, canPlayType() { return ''; },
    play() { return new Promise((resolve, reject) => pending.push({ resolve, reject })); },
  });
  element('#format').value = '/stream.aac?metadata=0';
  runInNewContext(script, {
    document: { querySelector: element },
    navigator: { userAgent: 'Desktop', maxTouchPoints: 0 },
  });
  return {
    audio, pending,
    play() { element('#toggle-play').dispatchEvent(new Event('click')); },
    changeFormat(path) {
      element('#format').value = path;
      element('#format').dispatchEvent(new Event('change'));
    },
    state() { return element('#status').dataset.state; },
    label() { return element('#toggle-play').attributes['aria-label']; },
  };
}

for (const name of ['AbortError', 'NotAllowedError', 'NotSupportedError']) {
  test(`current ${name} stops playback and shows an error`, async () => {
    const p = player();
    p.play();
    p.pending[0].reject(new DOMException('Playback failed', name));
    await setImmediate();
    assert.equal(p.state(), 'error');
    assert.equal(p.label(), 'Play');
    assert.equal(p.audio.src, undefined);
  });
}

test('stopping ignores a late playback rejection', async () => {
  const p = player();
  p.play();
  p.play();
  p.pending[0].reject(new DOMException('Cancelled', 'AbortError'));
  await setImmediate();
  assert.equal(p.state(), 'stopped');
  assert.equal(p.label(), 'Play');
  assert.equal(p.audio.src, undefined);
});

test('an old format failure cannot stop the replacement stream', async () => {
  const p = player();
  p.play();
  p.changeFormat('/stream.mp3');
  p.pending[0].reject(new Error('Old stream failed'));
  await setImmediate();
  assert.equal(p.state(), 'waiting');
  assert.equal(p.label(), 'Stop');
  assert.equal(p.audio.src, '/stream.mp3');
  p.pending[1].reject(new DOMException('New stream aborted', 'AbortError'));
  await setImmediate();
  assert.equal(p.state(), 'error');
  assert.equal(p.label(), 'Play');
  assert.equal(p.audio.src, undefined);
});
