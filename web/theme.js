// Pick the theme before the first paint: saved choice, else the system.
// A file of its own because the page's CSP allows no inline script.
(function () {
  var pref = "auto";
  try { pref = localStorage.getItem("groka.theme") || "auto"; } catch (e) {}
  var light = pref === "light" || (pref === "auto" && matchMedia("(prefers-color-scheme: light)").matches);
  document.documentElement.dataset.themePref = pref;
  document.documentElement.dataset.theme = light ? "light" : "dark";
})();
