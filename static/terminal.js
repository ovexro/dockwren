// Dockwren web terminal client: re-auth form, then xterm.js over a WebSocket.
(() => {
  const root = document.getElementById("term");
  const id = root.dataset.id;
  const form = document.getElementById("reauth");
  const status = document.getElementById("tstatus");
  const screen = document.getElementById("screen");
  const again = document.getElementById("again");
  const dark = matchMedia("(prefers-color-scheme: dark)").matches;
  const enc = new TextEncoder();
  let term = null, fit = null, ws = null, opened = false;

  function say(msg, bad) {
    status.textContent = msg;
    status.className = bad ? "bad" : "muted";
  }

  function start() {
    if (!term) {
      term = new Terminal({
        cursorBlink: true,
        fontSize: 14,
        fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
        theme: dark ? { background: "#0d1117", foreground: "#e6edf3" } : { background: "#ffffff", foreground: "#1f2328", cursor: "#1f2328", selectionBackground: "#b6d6ff" },
      });
      fit = new FitAddon.FitAddon();
      term.loadAddon(fit);
      term.open(screen);
      term.onData((d) => { if (ws && ws.readyState === 1) ws.send(enc.encode(d)); });
      term.onResize(({ cols, rows }) => sendSize(cols, rows));
      addEventListener("resize", () => fit.fit());
    } else {
      term.reset();
    }
    screen.hidden = false;
    form.hidden = true;
    again.hidden = true;
    fit.fit();
    opened = false;
    say("Connecting…");
    ws = new WebSocket((location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/containers/" + id + "/terminal/ws");
    ws.binaryType = "arraybuffer";
    ws.onopen = () => { opened = true; say("Connected."); sendSize(term.cols, term.rows); term.focus(); };
    ws.onmessage = (e) => term.write(new Uint8Array(e.data));
    ws.onclose = () => {
      if (opened) {
        say("Session ended.");
        again.hidden = false;
      } else {
        say("Could not open the terminal (limit of 3 sessions reached, permission expired, or container not running).", true);
        screen.hidden = true;
        form.hidden = false;
      }
    };
  }

  function sendSize(cols, rows) {
    if (ws && ws.readyState === 1) ws.send(JSON.stringify({ resize: [cols, rows] }));
  }

  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    say("Checking…");
    const res = await fetch("/containers/" + id + "/terminal/grant", {
      method: "POST",
      body: new URLSearchParams(new FormData(form)),
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
    });
    form.elements.password.value = "";
    if (form.elements.code) form.elements.code.value = "";
    if (res.status === 204) start();
    else if (res.status === 403) say("Not allowed: a terminal over a tunnel needs two-factor sign-in turned on.", true);
    else say("Wrong password or code.", true);
  });

  again.addEventListener("click", () => { form.hidden = false; again.hidden = true; screen.hidden = true; say("Confirm your password to reconnect."); });
})();
