const mcpCatalog = { services: [], editing: null, busy: false };
const mcpGrants = new Map();

function mcpError(message = "") {
  $("mcp-error").textContent = message;
  $("mcp-error").hidden = !message;
}

function showMcpForm(service) {
  mcpCatalog.editing = service?.id || null;
  $("mcp-catalogue").hidden = true;
  $("mcp-form").hidden = false;
  $("mcp-title").textContent = service ? "Edit MCP service" : "Add MCP service";
  $("mcp-name").value = service?.name || "";
  $("mcp-transport").value = service?.transport || "http";
  $("mcp-url").value = service?.url || "";
  $("mcp-bearer").value = service?.bearerFile || "";
  $("mcp-command").value = service?.command || "";
  $("mcp-args").value = (service?.args || []).join("\n");
  $("mcp-env").value = Object.entries(service?.envFiles || {}).map(([key, file]) => key + "=" + file).join("\n");
  $("mcp-enabled").checked = service?.enabled ?? true;
  mcpError();
  showMcpTransport();
  $("mcp-name").focus();
}

function showMcpTransport() {
  const http = $("mcp-transport").value === "http";
  $("mcp-http").hidden = !http;
  $("mcp-stdio").hidden = http;
  $("mcp-url").disabled = !http;
  $("mcp-url").required = http;
  $("mcp-command").disabled = http;
  $("mcp-command").required = !http;
}

function showMcpCatalog() {
  $("mcp-title").textContent = "MCP Services";
  $("mcp-form").hidden = true;
  $("mcp-catalogue").hidden = false;
  $("mcp-list").innerHTML = mcpCatalog.services.map((s) => `
    <div class="mcp-service">
      <div class="mcp-service-name">${esc(s.name)}</div>
      <div class="d-sub">${s.transport === "http" ? "Streamable HTTP" : "Local stdio"} · ${s.enabled ? "Enabled" : "Disabled"}</div>
      <div class="d-sub" role="status">${esc(s.status || "Connection untested")}${s.testedAt ? " · " + esc(when(s.testedAt)) : ""}</div>
      <div class="mcp-actions">
        <button type="button" data-mcp-action="test" data-service="${s.id}">Test connection</button>
        <button type="button" data-mcp-action="edit" data-service="${s.id}">Edit</button>
        <button type="button" class="d-danger" data-mcp-action="remove" data-service="${s.id}">Remove</button>
      </div>
    </div>`).join("") || '<p class="d-empty">No MCP services configured.</p>';
}

async function openMcp() {
  $("mcp-dialog").showModal();
  if (mcpCatalog.busy) return;
  mcpError();
  mcpCatalog.services = [];
  showMcpCatalog();
  $("mcp-list").textContent = "Loading MCP services…";
  $("mcp-add").disabled = true;
  try {
    mcpCatalog.services = await api("GET", "/api/mcp/services");
    $("mcp-add").disabled = false;
    showMcpCatalog();
  } catch (err) {
    $("mcp-list").textContent = "";
    mcpError(err.message);
  }
}

function readMcpForm() {
  const transport = $("mcp-transport").value;
  const settings = { name: $("mcp-name").value, transport, enabled: $("mcp-enabled").checked };
  if (transport === "http") {
    return { ...settings, url: $("mcp-url").value, bearerFile: $("mcp-bearer").value.trim() || null };
  }
  const env = $("mcp-env").value.split("\n").filter((line) => line.trim()).map((line) => {
    const [key, file, extra] = line.split("=");
    if (!key?.trim() || !file?.trim() || extra !== undefined) throw new Error("Use VARIABLE=filename for each credential file.");
    return [key.trim(), file.trim()];
  });
  const lines = $("mcp-args").value;
  return { ...settings, command: $("mcp-command").value, args: lines ? lines.split("\n") : [], envFiles: Object.fromEntries(env) };
}

function refreshVisibleGrants() {
  mcpGrants.clear();
  if (details.open && details.tab === "settings") loadMcpGrants(activeId);
}

$("mcp-open").addEventListener("click", openMcp);
$("mcp-close").addEventListener("click", () => $("mcp-dialog").close());
$("mcp-add").addEventListener("click", () => showMcpForm());
$("mcp-cancel").addEventListener("click", () => { mcpError(); showMcpCatalog(); $("mcp-add").focus(); });
$("mcp-transport").addEventListener("change", showMcpTransport);
$("mcp-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (mcpCatalog.busy) return;
  mcpError();
  mcpCatalog.busy = $("mcp-save").disabled = true;
  try {
    const id = mcpCatalog.editing;
    const service = await api(id ? "PATCH" : "POST", id ? "/api/mcp/services/" + id : "/api/mcp/services", readMcpForm());
    mcpCatalog.services = mcpCatalog.services.filter((s) => s.id !== id).concat(service);
    mcpCatalog.services.sort((a, b) => a.name.localeCompare(b.name));
    showMcpCatalog();
    refreshVisibleGrants();
    $("mcp-add").focus();
  } catch (err) {
    mcpError(err.message);
  } finally {
    mcpCatalog.busy = $("mcp-save").disabled = false;
  }
});

$("mcp-list").addEventListener("click", async (event) => {
  const button = event.target.closest("[data-mcp-action]");
  if (!button || mcpCatalog.busy) return;
  const service = mcpCatalog.services.find((s) => s.id === button.dataset.service);
  if (!service) return;
  const action = button.dataset.mcpAction;
  if (action === "edit") return showMcpForm(service);
  if (action === "remove" && !confirm("Remove " + service.name + " and its bot assignments?")) return;
  mcpError();
  button.disabled = mcpCatalog.busy = $("mcp-add").disabled = true;
  if (action === "test") button.textContent = "Testing…";
  const path = "/api/mcp/services/" + service.id;
  try {
    if (action === "test") {
      Object.assign(service, await api("POST", path + "/test", {}));
    } else {
      await api("DELETE", path);
      mcpCatalog.services = mcpCatalog.services.filter((s) => s.id !== service.id);
      refreshVisibleGrants();
    }
  } catch (err) {
    mcpError(err.message);
  } finally {
    mcpCatalog.busy = $("mcp-add").disabled = false;
    showMcpCatalog();
    $("mcp-list").querySelector('[data-mcp-action="' + action + '"][data-service="' + service.id + '"]')?.focus();
  }
});

function renderMcpGrants(botId) {
  const entry = mcpGrants.get(botId);
  if (!entry?.services && !entry?.error) return '<div class="d-item d-empty">Loading services…</div>';
  if (entry.error) return '<div class="d-item d-empty">' + esc(entry.error) + '</div>';
  if (!entry.services.length) return '<div class="d-item d-empty">No MCP services configured.</div>';
  return entry.services.map((s) => `
    <div class="d-item"><div class="d-text"><div>${esc(s.name)}</div>${s.enabled ? "" : '<div class="d-sub">Disabled globally</div>'}</div>
      <button type="button" role="switch" aria-label="${esc(s.name)}" aria-checked="${s.granted}" class="switch ${s.granted ? "on" : ""}"
        data-mcp-grant="${s.id}" ${entry.pending === s.id ? "disabled" : ""}><span></span></button>
    </div>`).join("");
}

async function loadMcpGrants(botId) {
  const entry = mcpGrants.get(botId) || {};
  mcpGrants.set(botId, entry);
  try { entry.services = await api("GET", "/api/bots/" + botId + "/mcp"); delete entry.error; }
  catch (err) { entry.error = err.message; }
  if (activeId === botId && mcpGrants.get(botId) === entry && $("mcp-grants")) {
    $("mcp-grants").innerHTML = renderMcpGrants(botId);
  }
}

async function toggleMcpGrant(botId, id) {
  const entry = mcpGrants.get(botId);
  const service = entry?.services?.find((s) => s.id === id);
  if (!service || entry.pending) return;
  entry.pending = id;
  const selector = '[data-mcp-grant="' + id + '"]';
  const button = $("mcp-grants")?.querySelector(selector);
  if (button) button.disabled = true;
  try {
    const result = await api("PUT", "/api/bots/" + botId + "/mcp/" + id, { granted: !service.granted });
    service.granted = result.granted;
  } catch (err) {
    alert(err.message);
  } finally {
    delete entry.pending;
    if (activeId === botId && mcpGrants.get(botId) === entry && $("mcp-grants")) {
      $("mcp-grants").innerHTML = renderMcpGrants(botId);
      $("mcp-grants").querySelector(selector)?.focus();
    }
  }
}
