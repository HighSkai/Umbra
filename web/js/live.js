// Live view, loaded only when you press "Live".
// Ring: WebRTC straight from Ring's servers to this browser; Umbra relays signaling.
// Blink: fragmented MP4 repackaged by ffmpeg on the server, played with Media Source Extensions.
// Everything stops when the dialog closes, the tab is hidden, or the server's time limit hits.

let current = null;

function el(tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
}

function post(url, headers, body) {
  return fetch(url, {
    method: 'POST',
    headers: Object.assign({ 'Content-Type': 'application/json' }, headers),
    body: body ? JSON.stringify(body) : undefined,
    credentials: 'same-origin',
  });
}

function dialog(name, source) {
  const dlg = el('dialog', 'liveview');
  dlg.setAttribute('aria-label', 'Live view: ' + name);
  const head = el('div', 'lv-head');
  const title = el('div', 'lv-title');
  title.append(el('span', 'lv-live', 'LIVE'), el('span', null, ''));
  title.lastChild.textContent = name;
  const close = el('button', 'btn btn-small btn-ghost', 'Close');
  close.type = 'button';
  head.append(title, close);
  const stage = el('div', 'lv-stage');
  const video = el('video');
  video.autoplay = true;
  video.muted = true;
  video.playsInline = true;
  video.controls = true;
  const msg = el('div', 'lv-msg', 'Connecting…');
  stage.append(video, msg);
  const foot = el('div', 'lv-foot');
  foot.append(el('span', null, source === 'ring' ? 'Ring · video comes straight from Ring' : 'Blink · repackaged on the server, not re-encoded'), el('span', 'lv-timer mono', ''));
  dlg.append(head, stage, foot);
  document.body.append(dlg);
  return { dlg, video, msg, close, timer: foot.lastChild };
}

export function open(source, device, name, headers) {
  if (current) current.stop();
  const ui = dialog(name || 'Camera', source);
  const started = Date.now();
  let sid = null;
  let pc = null;
  let es = null;
  let keep = 0;
  let tick = 0;
  let stopped = false;
  const ctrl = new AbortController();

  const stop = () => {
    if (stopped) return;
    stopped = true;
    clearInterval(keep);
    clearInterval(tick);
    ctrl.abort();
    if (es) es.close();
    if (pc) pc.close();
    try { ui.video.pause(); ui.video.removeAttribute('src'); ui.video.load(); } catch (_) {}
    if (sid) post('/live/s/' + sid + '/stop', headers).catch(() => {});
    document.removeEventListener('visibilitychange', onVis);
    if (ui.dlg.open) ui.dlg.close();
    ui.dlg.remove();
    if (current && current.stop === stop) current = null;
  };
  const fail = (text) => {
    ui.msg.hidden = false;
    ui.msg.textContent = text;
  };
  const onVis = () => { if (document.visibilityState === 'hidden') stop(); };
  document.addEventListener('visibilitychange', onVis);
  ui.close.addEventListener('click', stop);
  ui.dlg.addEventListener('close', stop);
  ui.dlg.showModal();
  current = { stop };
  tick = setInterval(() => {
    const s = Math.floor((Date.now() - started) / 1000);
    ui.timer.textContent = Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0');
  }, 1000);

  post('/live/' + source + '/' + encodeURIComponent(device) + '/start', headers)
    .then((r) => r.json().then((j) => ({ ok: r.ok, j })))
    .then(({ ok, j }) => {
      if (stopped) return;
      if (!ok) throw new Error(j.error || 'Live view failed to start');
      sid = j.session;
      keep = setInterval(() => post('/live/s/' + sid + '/keepalive', headers).catch(() => {}), 30000);
      return j.kind === 'webrtc' ? webrtc(j) : mse(j);
    })
    .catch((e) => fail(e.message || String(e)));

  async function webrtc(j) {
    const ice = (j.ice_servers || []).map((u) => ({ urls: u }));
    pc = new RTCPeerConnection({ iceServers: ice });
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });
    pc.ontrack = (ev) => {
      if (!ui.video.srcObject) ui.video.srcObject = ev.streams[0] || new MediaStream([ev.track]);
      ui.msg.hidden = true;
    };
    pc.onicecandidate = (ev) => {
      if (ev.candidate && ev.candidate.candidate) {
        post('/live/s/' + sid + '/candidate', headers, { candidate: ev.candidate.candidate, mline: ev.candidate.sdpMLineIndex || 0 }).catch(() => {});
      }
    };
    pc.onconnectionstatechange = () => {
      if (pc && (pc.connectionState === 'failed' || pc.connectionState === 'closed')) fail('The connection to Ring dropped.');
    };
    es = new EventSource('/live/s/' + sid + '/events');
    es.addEventListener('answer', (e) => {
      const m = JSON.parse(e.data);
      pc.setRemoteDescription({ type: 'answer', sdp: m.sdp }).catch((err) => fail('Ring sent an answer this browser could not use: ' + err.message));
    });
    es.addEventListener('candidate', (e) => {
      const m = JSON.parse(e.data);
      pc.addIceCandidate({ candidate: m.candidate, sdpMLineIndex: m.mline || 0 }).catch(() => {});
    });
    es.addEventListener('error', (e) => {
      if (e.data) { try { fail(JSON.parse(e.data).message || 'Ring ended the stream.'); } catch (_) {} }
    });
    es.addEventListener('closed', () => fail('The live view ended.'));
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await post('/live/s/' + sid + '/offer', headers, { sdp: offer.sdp });
  }

  async function mse(j) {
    const MS = window.ManagedMediaSource || window.MediaSource;
    if (!MS) return fail('This browser cannot play live video (no Media Source Extensions).');
    const ms = new MS();
    ui.video.disableRemotePlayback = true;
    ui.video.src = URL.createObjectURL(ms);
    await new Promise((res) => ms.addEventListener('sourceopen', res, { once: true }));
    const resp = await fetch(j.stream, { credentials: 'same-origin', signal: ctrl.signal });
    if (!resp.ok || !resp.body) {
      let t = 'The stream did not start.';
      try { t = (await resp.json()).error || t; } catch (_) {}
      return fail(t);
    }
    const reader = resp.body.getReader();
    let buf = new Uint8Array(0);
    let sb = null;
    const queue = [];
    const pump = () => {
      if (!sb || sb.updating || !queue.length) return;
      try { sb.appendBuffer(queue.shift()); } catch (e) { fail('Playback error: ' + e.message); }
    };
    for (;;) {
      const { value, done } = await reader.read();
      if (done || stopped) break;
      if (!sb) {
        buf = concat(buf, value);
        const init = initSegment(buf);
        if (!init) continue;
        const type = 'video/mp4; codecs="' + codecs(buf.subarray(0, init)) + '"';
        if (!MS.isTypeSupported(type)) return fail('This browser cannot play the camera format (' + type + ').');
        sb = ms.addSourceBuffer(type);
        sb.mode = 'segments';
        sb.addEventListener('updateend', () => {
          // Stay near the live edge and keep the buffer short.
          const v = ui.video;
          if (v.buffered.length) {
            const end = v.buffered.end(v.buffered.length - 1);
            if (end - v.currentTime > 3) v.currentTime = end - 0.5;
            if (!sb.updating && v.currentTime > 30 && v.buffered.start(0) < v.currentTime - 20) {
              try { sb.remove(0, v.currentTime - 15); return; } catch (_) {}
            }
          }
          pump();
        });
        queue.push(buf);
        buf = null;
        ui.msg.hidden = true;
        ui.video.play().catch(() => {});
      } else {
        queue.push(value);
      }
      pump();
    }
    if (!stopped) fail('The live view ended.');
  }
}

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

// Byte length of ftyp+moov once both are fully received, else 0.
function initSegment(u8) {
  let off = 0;
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  while (off + 8 <= u8.length) {
    const size = dv.getUint32(off);
    const type = String.fromCharCode(u8[off + 4], u8[off + 5], u8[off + 6], u8[off + 7]);
    if (size < 8 || off + size > u8.length) return 0;
    off += size;
    if (type === 'moov') return off;
  }
  return 0;
}

// Codec string from the avcC box (e.g. avc1.4d401f), plus AAC if there is audio.
function codecs(init) {
  const s = [];
  for (let i = 4; i + 8 < init.length; i++) {
    if (init[i] === 0x61 && init[i + 1] === 0x76 && init[i + 2] === 0x63 && init[i + 3] === 0x43) {
      const hex = (n) => n.toString(16).padStart(2, '0');
      s.push('avc1.' + hex(init[i + 5]) + hex(init[i + 6]) + hex(init[i + 7]));
      break;
    }
  }
  if (!s.length) s.push('avc1.42e01f');
  for (let i = 4; i + 4 < init.length; i++) {
    if (init[i] === 0x6d && init[i + 1] === 0x70 && init[i + 2] === 0x34 && init[i + 3] === 0x61) { s.push('mp4a.40.2'); break; }
  }
  return s.join(',');
}
