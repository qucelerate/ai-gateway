// Puts the remembered theme's Carbon zone on <html> before the first paint,
// so a dark theme never flashes light. A file rather than an inline script,
// since the CSP allows only same-origin scripts. theme.ts does the same once
// the app loads; theme.test.ts checks the two agree.
(function () {
  var dark = false;
  try {
    dark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  } catch (e) {}
  var theme = dark ? 'dark' : 'light';
  try {
    var stored = window.localStorage.getItem('gateway.admin.theme');
    if (stored === 'light' || stored === 'dark') theme = stored;
  } catch (e) {}
  var root = document.documentElement;
  root.classList.add(theme === 'dark' ? 'cds--g100' : 'cds--white');
  root.setAttribute('data-theme', theme);
})();
