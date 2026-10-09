import assert from 'node:assert/strict';
import { test } from 'node:test';

import { createRealtime } from '../js/realtime.js';

class FakeSocket {
  static instances = [];
  constructor(url) {
    this.url = url;
    this.sent = [];
    this.readyState = 0;
    FakeSocket.instances.push(this);
  }
  send(data) {
    this.sent.push(JSON.parse(data));
  }
  close() {
    this.readyState = 3;
    this.onclose?.();
  }
  // helpers for the test
  open() {
    this.readyState = 1;
    return this.onopen();
  }
  receive(msg) {
    return this.onmessage({ data: JSON.stringify(msg) });
  }
}

function setup(extra = {}) {
  FakeSocket.instances = [];
  const timers = [];
  const rt = createRealtime({
    getUrl: () => 'ws://x/websocket',
    getToken: async () => 'tok-1',
    WebSocketImpl: FakeSocket,
    setTimeoutFn: (fn, ms) => timers.push({ fn, ms }) && timers.length,
    clearTimeoutFn: () => {},
    ...extra,
  });
  return { rt, timers, sockets: FakeSocket.instances };
}

test('signs in first, then sends the subscriptions, and dispatches events by uid', async () => {
  const { rt, sockets } = setup();
  const seen = [];
  const stop = rt.subscribe('pages', (m) => seen.push(m), { event: 'update', fields: ['title'], filter: { status: { _eq: 'x' } } });
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].url, 'ws://x/websocket');
  await sockets[0].open();
  assert.deepEqual(sockets[0].sent[0], { type: 'auth', access_token: 'tok-1' });
  assert.deepEqual(sockets[0].sent[1], {
    type: 'subscribe',
    uid: 's1',
    collection: 'pages',
    event: 'update',
    query: { fields: ['title'], filter: { status: { _eq: 'x' } } },
  });

  await sockets[0].receive({ type: 'subscription', event: 'init', uid: 's1' });
  assert.equal(seen.length, 0, 'init is not an event');
  await sockets[0].receive({ type: 'subscription', event: 'update', uid: 's1', data: [{ title: 'T' }] });
  await sockets[0].receive({ type: 'subscription', event: 'update', uid: 'other', data: [] });
  assert.deepEqual(seen, [{ event: 'update', data: [{ title: 'T' }], truncated: false }]);

  stop();
  assert.equal(rt.connected, false, 'closed with the last subscription');
});

test('a second subscription reuses the open socket; unsubscribing one keeps the other', async () => {
  const { rt, sockets } = setup();
  const a = rt.subscribe('pages', () => {});
  await sockets[0].open();
  rt.subscribe('tags', () => {});
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].sent.at(-1).collection, 'tags');
  a();
  assert.deepEqual(sockets[0].sent.at(-1), { type: 'unsubscribe', uid: 's1' });
  assert.equal(rt.connected, true);
});

test('reconnects with backoff, signs in again, resubscribes and asks for a resync', async () => {
  const { rt, timers, sockets } = setup();
  const seen = [];
  rt.subscribe('pages', (m) => seen.push(m.event));
  await sockets[0].open();

  sockets[0].close(); // connection lost
  assert.equal(timers.length, 1);
  assert.equal(timers[0].ms, 1000);
  timers[0].fn();
  assert.equal(sockets.length, 2);
  await sockets[1].open();
  assert.equal(sockets[1].sent[0].type, 'auth');
  assert.equal(sockets[1].sent[1].type, 'subscribe');
  assert.deepEqual(seen, ['resync'], 'the view must refetch: events may have been missed');

  sockets[1].close();
  assert.equal(timers.at(-1).ms, 1000, 'backoff restarts after a successful connection');
  timers.at(-1).fn();
  sockets[2].close(); // fails before opening
  assert.equal(timers.at(-1).ms, 2000);
});

test('an authentication error refreshes the token and reconnects', async () => {
  let refreshed = 0;
  const { rt, timers, sockets } = setup({ refreshToken: async () => refreshed++ });
  rt.subscribe('pages', () => {});
  await sockets[0].open();
  await sockets[0].receive({ type: 'auth', status: 'error', code: 'UNAUTHORIZED' });
  assert.equal(refreshed, 1);
  assert.equal(sockets[0].readyState, 3);
  assert.equal(timers.length, 1, 'reconnection scheduled');
});

test('server-side subscription errors reach the handler; close() forgets everything', async () => {
  const { rt, timers, sockets } = setup();
  const seen = [];
  rt.subscribe('nope', (m) => seen.push(m));
  await sockets[0].open();
  await sockets[0].receive({ type: 'error', uid: 's1', code: 'NOT_FOUND', message: 'collection not found' });
  assert.deepEqual(seen, [{ event: 'error', code: 'NOT_FOUND', message: 'collection not found' }]);
  rt.close();
  assert.equal(timers.length, 0, 'no reconnection after an explicit close');
  assert.equal(rt.connected, false);
});

test('a failing handler does not break the others', async () => {
  const { rt, sockets } = setup();
  const seen = [];
  rt.subscribe('pages', () => {
    throw new Error('boom');
  });
  rt.subscribe('pages', (m) => seen.push(m.event));
  await sockets[0].open();
  const originalError = console.error;
  console.error = () => {};
  await sockets[0].receive({ type: 'subscription', event: 'create', uid: 's1', data: [] });
  await sockets[0].receive({ type: 'subscription', event: 'create', uid: 's2', data: [] });
  console.error = originalError;
  assert.deepEqual(seen, ['create']);
});
