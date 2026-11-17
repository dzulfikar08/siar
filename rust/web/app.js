(function () {
  // Tabs: without JS every panel shows, stacked under its own heading.
  document.querySelectorAll(".tabs").forEach(function (box, n) {
    var panels = box.querySelectorAll(".tabpanel");
    var list = box.querySelector(".tablist");
    if (!list || !panels.length) return;
    var tabs = [];
    panels.forEach(function (p, i) {
      var b = document.createElement("button");
      b.type = "button";
      b.setAttribute("role", "tab");
      b.id = "tab-" + n + "-" + i;
      p.id = p.id || "panel-" + n + "-" + i;
      b.setAttribute("aria-controls", p.id);
      p.setAttribute("role", "tabpanel");
      p.setAttribute("aria-labelledby", b.id);
      p.tabIndex = 0;
      b.textContent = p.getAttribute("data-tab");
      b.addEventListener("click", function () { pick(i, false); });
      b.addEventListener("keydown", function (e) {
        var k = e.key, j = i;
        if (k === "ArrowRight") j = (i + 1) % tabs.length;
        else if (k === "ArrowLeft") j = (i - 1 + tabs.length) % tabs.length;
        else if (k === "Home") j = 0;
        else if (k === "End") j = tabs.length - 1;
        else return;
        e.preventDefault();
        pick(j, true);
      });
      list.appendChild(b);
      tabs.push(b);
    });
    function pick(i, focus) {
      tabs.forEach(function (t, j) {
        var on = i === j;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        panels[j].hidden = !on;
      });
      if (focus) tabs[i].focus();
    }
    list.setAttribute("role", "tablist");
    box.classList.add("ready");
    pick(0, false);
  });

  // Copy buttons on code blocks.
  if (navigator.clipboard) {
    document.querySelectorAll(".code pre").forEach(function (pre) {
      var b = document.createElement("button");
      b.type = "button";
      b.className = "copy";
      b.textContent = "Copy";
      b.setAttribute("aria-label", "Copy " + (pre.getAttribute("data-what") || "code"));
      b.addEventListener("click", function () {
        navigator.clipboard.writeText(pre.innerText.replace(/\n$/, "")).then(function () {
          b.textContent = "Copied";
          setTimeout(function () { b.textContent = "Copy"; }, 1600);
        }, function () {
          b.textContent = "Select and copy";
        });
      });
      pre.style.paddingRight = "5rem";
      pre.parentNode.appendChild(b);
    });
  }
})();

// Open a <details> section when a link points at it.
(function () {
  function open() {
    var el = location.hash && document.getElementById(decodeURIComponent(location.hash.slice(1)));
    if (el && el.tagName === "DETAILS") el.open = true;
  }
  window.addEventListener("hashchange", open);
  open();
})();
