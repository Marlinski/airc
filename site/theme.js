// AIRC Theme Switcher
// Toggles between style.css (retro/geocities) and style-modern.css
// Persists choice in localStorage.
(function () {
  var STORAGE_KEY = "airc-theme";
  var RETRO = "retro";
  var MODERN = "modern";

  var link = document.getElementById("theme-css");
  var saved = localStorage.getItem(STORAGE_KEY) || RETRO;

  function apply(theme) {
    link.href = theme === MODERN ? "style-modern.css" : "style.css";
    localStorage.setItem(STORAGE_KEY, theme);
    updateButtons(theme);
  }

  function updateButtons(theme) {
    var btns = document.querySelectorAll(".theme-toggle");
    for (var i = 0; i < btns.length; i++) {
      btns[i].textContent = theme === MODERN ? "Go Retro" : "Go Modern";
    }
  }

  function toggle() {
    var current = localStorage.getItem(STORAGE_KEY) || RETRO;
    apply(current === MODERN ? RETRO : MODERN);
  }

  // Apply stylesheet immediately (before render)
  link.href = saved === MODERN ? "style-modern.css" : "style.css";
  localStorage.setItem(STORAGE_KEY, saved);

  // Update button labels once DOM is ready
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function () {
      updateButtons(saved);
    });
  } else {
    updateButtons(saved);
  }

  // Wire up toggle buttons via event delegation
  document.addEventListener("click", function (e) {
    if (e.target.classList.contains("theme-toggle")) {
      toggle();
    }
  });
})();
