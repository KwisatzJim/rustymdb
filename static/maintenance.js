async function pollMaintenance() {
  const panel = document.getElementById("maintenance");
  if (!panel || panel.dataset.running !== "true") return;
  try {
    const response = await fetch("/maintenance/status", { cache: "no-store" });
    if (!response.ok) throw new Error("Status unavailable");
    panel.outerHTML = await response.text();
  } catch {
    const message = panel.querySelector(".maintenance-connection");
    if (message) {
      message.hidden = false;
      message.textContent = "Status connection unavailable. Retrying shortly…";
    }
  }
  setTimeout(pollMaintenance, 3000);
}
pollMaintenance();
