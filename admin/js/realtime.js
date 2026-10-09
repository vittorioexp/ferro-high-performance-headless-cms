// Realtime client for GET /websocket.
//
//   const stop = realtime.subscribe('pages', (msg) => console.log(msg.event, msg.data),
//                                   { event: 'update', fields: ['title'], filter: { status: { _eq: 'published' } } });
//   stop();
//
// `msg.event` is create | update | delete | resync. `resync` means events may have been
// missed (the connection dropped, or the server was overloaded): refetch what you show.
// The socket is opened by the first subscription and closed with the last one. It reconnects with
// exponential backoff (1 s .. 30 s), signs in again with a fresh token and re-sends every subscription.

import { baseUrl, currentToken, refreshSession } from './api.js';

const OPEN = 1;

export function createRealtime({ getUrl, getToken, refreshToken, WebSocketImpl, setTimeoutFn = setTimeout, clearTimeoutFn = clearTimeout } = {}) {
  /** uid -> { collection, opts, handler } */
  const subs = new Map();
  let ws = null;
  let seq = 0;
  let retry = 0;
  let timer = null;
  let everConnected = false;

  const send = (o) => {
    if (ws && ws.readyState === OPEN) ws.send(JSON.stringify(o));
  };

  const sendSubscribe = (uid, s) =>
    send({
      type: 'subscribe',
      uid,
      collection: s.collection,
      ...(s.opts.event ? { event: s.opts.event } : {}),
      query: { ...(s.opts.fields ? { fields: s.opts.fields } : {}), ...(s.opts.filter ? { filter: s.opts.filter } : {}) },
    });

  const emit = (s, msg) => {
    try {
      s.handler(msg);
    } catch (err) {
      console.error('realtime handler failed:', err);
    }
  };

  function scheduleReconnect() {
    if (timer || subs.size === 0) return;
    const delay = Math.min(30000, 1000 * 2 ** retry++);
    timer = setTimeoutFn(() => {
      timer = null;
      connect();
    }, delay);
  }

  function connect() {
    if (ws || subs.size === 0) return;
    const Impl = WebSocketImpl || globalThis.WebSocket;
    const socket = new Impl(getUrl());
    ws = socket;
    socket.onopen = async () => {
      retry = 0;
      const token = await getToken();
      if (ws !== socket) return;
      if (token) send({ type: 'auth', access_token: token });
      const reconnected = everConnected;
      everConnected = true;
      for (const [uid, s] of subs) {
        sendSubscribe(uid, s);
        if (reconnected) emit(s, { event: 'resync', data: null });
      }
    };
    socket.onmessage = async (e) => {
      let m;
      try {
        m = JSON.parse(e.data);
      } catch {
        return;
      }
      if (m.type === 'subscription' && m.uid && subs.has(m.uid)) {
        if (m.event !== 'init' && m.event !== 'unsubscribe') emit(subs.get(m.uid), { event: m.event, data: m.data, truncated: !!m.truncated });
      } else if (m.type === 'error' && m.uid && subs.has(m.uid)) {
        emit(subs.get(m.uid), { event: 'error', code: m.code, message: m.message });
      } else if (m.type === 'auth' && m.status === 'error') {
        // expired or revoked token: get a new one, then reconnect
        if (refreshToken) await refreshToken();
        socket.close();
      }
    };
    socket.onclose = () => {
      if (ws === socket) ws = null;
      scheduleReconnect();
    };
    socket.onerror = () => {};
  }

  return {
    /** Returns the function that cancels the subscription. */
    subscribe(collection, handler, opts = {}) {
      const uid = `s${++seq}`;
      subs.set(uid, { collection, opts, handler });
      if (ws && ws.readyState === OPEN) sendSubscribe(uid, subs.get(uid));
      else connect();
      return () => {
        if (!subs.delete(uid)) return;
        send({ type: 'unsubscribe', uid });
        if (subs.size === 0) this.close();
      };
    },
    /** Close the socket and forget the subscriptions (sign-out). */
    close() {
      subs.clear();
      if (timer) clearTimeoutFn(timer);
      timer = null;
      retry = 0;
      everConnected = false;
      const socket = ws;
      ws = null;
      if (socket) socket.close();
    },
    get connected() {
      return !!ws && ws.readyState === OPEN;
    },
  };
}

function wsUrl() {
  const base = baseUrl() || globalThis.location?.origin || '';
  return `${base.replace(/^http/, 'ws')}/websocket`;
}

/** The app-wide instance. */
export const realtime = createRealtime({ getUrl: wsUrl, getToken: async () => currentToken(), refreshToken: refreshSession });
