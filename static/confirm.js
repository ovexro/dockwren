// Styled confirmation dialog for every hx-confirm, instead of the browser's confirm() pop-up.
(() => {
  let dlg;
  const build = () => {
    dlg = document.createElement('dialog');
    dlg.className = 'confirm';
    dlg.setAttribute('aria-labelledby', 'confirm-msg');
    dlg.innerHTML = '<form method="dialog"><p id="confirm-msg"></p><div class="confirm-actions">' +
      '<button type="submit" value="cancel" class="confirm-cancel">Cancel</button>' +
      '<button type="submit" value="ok" class="confirm-ok primary"></button></div></form>';
    document.body.appendChild(dlg);
    return dlg;
  };
  document.addEventListener('htmx:confirm', (e) => {
    if (!e.detail.question) return;
    e.preventDefault();
    const d = dlg || build();
    const el = e.detail.elt;
    const danger = !!el && (el.matches('.danger') || !!el.querySelector('button.danger'));
    const label = (el && (el.matches('button') ? el : el.querySelector('button[type=submit], button:not([type])')))?.textContent.trim();
    d.querySelector('#confirm-msg').textContent = e.detail.question;
    const ok = d.querySelector('.confirm-ok');
    ok.textContent = label && label.length <= 28 ? label : 'Confirm';
    ok.classList.toggle('danger', danger);
    ok.classList.toggle('primary', !danger);
    d.returnValue = 'cancel';
    d.onclose = () => { if (d.returnValue === 'ok') e.detail.issueRequest(true); };
    d.showModal();
    d.querySelector('.confirm-cancel').focus();
  });
})();

// "More" menu on app rows: close after choosing an action or clicking elsewhere.
document.addEventListener("click", function (e) {
  var inside = e.target.closest && e.target.closest("details.more");
  document.querySelectorAll("details.more[open]").forEach(function (d) {
    if (d !== inside || (e.target.closest("button") && d.contains(e.target))) d.removeAttribute("open");
  });
});
