// Passkey (WebAuthn) client: registration in Settings, sign-in on the login page.
(() => {
  const enc = (buf) =>
    btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const dec = (s) => {
    s = s.replace(/-/g, '+').replace(/_/g, '/');
    s += '='.repeat((4 - (s.length % 4)) % 4);
    return Uint8Array.from(atob(s), (c) => c.charCodeAt(0)).buffer;
  };
  const post = async (url, body) => {
    const r = await fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    });
    return r.json().catch(() => ({ ok: false, error: 'Unexpected answer from the panel.' }));
  };
  const say = (el, text) => {
    if (el) el.textContent = text;
  };
  const fail = (e) => (e && e.name === 'NotAllowedError' ? 'Cancelled or timed out.' : (e && e.message) || 'Failed.');

  async function add(btn) {
    const msg = document.querySelector('[data-pk-msg]');
    if (!window.PublicKeyCredential) return say(msg, 'This browser has no passkey support.');
    btn.disabled = true;
    try {
      const o = await post('/settings/passkeys/options', {});
      if (o.ok === false) return say(msg, o.error);
      const pk = o.publicKey;
      pk.challenge = dec(pk.challenge);
      pk.user.id = dec(pk.user.id);
      pk.excludeCredentials = pk.excludeCredentials.map((c) => ({ ...c, id: dec(c.id) }));
      const cred = await navigator.credentials.create({ publicKey: pk });
      const name = (document.querySelector('[data-pk-name]') || {}).value || '';
      const res = await post('/settings/passkeys/register', {
        pending: o.pending,
        id: enc(cred.rawId),
        clientDataJSON: enc(cred.response.clientDataJSON),
        attestationObject: enc(cred.response.attestationObject),
        name,
      });
      if (!res.ok) return say(msg, res.error);
      const box = document.getElementById('passkeys');
      box.outerHTML = res.html;
    } catch (e) {
      say(msg, fail(e));
    } finally {
      btn.disabled = false;
    }
  }

  async function login(root) {
    const msg = root.querySelector('[data-pk-msg]');
    const o = JSON.parse(root.dataset.options);
    if (!window.PublicKeyCredential) return say(msg, 'This browser has no passkey support. Use a code below.');
    const pk = o.publicKey;
    pk.challenge = dec(pk.challenge);
    pk.allowCredentials = pk.allowCredentials.map((c) => ({ ...c, id: dec(c.id) }));
    try {
      const cred = await navigator.credentials.get({ publicKey: pk });
      const res = await post('/login/passkey', {
        pending: o.pending,
        id: enc(cred.rawId),
        clientDataJSON: enc(cred.response.clientDataJSON),
        authenticatorData: enc(cred.response.authenticatorData),
        signature: enc(cred.response.signature),
      });
      if (res.ok) location.href = '/';
      else say(msg, (res.error || 'Failed.') + ' Start again from the password step.');
    } catch (e) {
      say(msg, fail(e) + ' Use the button to try again, or a code below.');
    }
  }

  document.addEventListener('click', (ev) => {
    const a = ev.target.closest('[data-pk-add]');
    if (a) add(a);
    const l = ev.target.closest('[data-pk-login]');
    if (l) login(document.getElementById('pk-login'));
  });
  window.addEventListener('DOMContentLoaded', () => {
    const root = document.getElementById('pk-login');
    if (root) login(root);
  });
})();
