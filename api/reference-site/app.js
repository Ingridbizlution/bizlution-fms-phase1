// FMS API 參考文件 —— 純前端渲染，零框架、零建置工具。
//
// 資料來源是 build.py 產生好的 data/openapi.json（openapi.yaml 轉 JSON，
// 每個 description 都已經附上渲染好的 description_html）與
// data/getting-started.html（FRONTEND-GETTING-STARTED.md 轉好的 HTML）。
// 這支檔案只做 DOM 渲染與 JSON 走訪，不解析 YAML 也不解析 Markdown——
// 那些工作留在 build.py，瀏覽器端因此不需要任何 vendored 解析器。
//
// 路由用 location.hash（#/getting-started、#/operations/{operationId}），
// 不是 History API pushState：這是純靜態檔案（GitHub Pages 或直接開
// index.html），hash 不需要伺服器端 fallback 規則就能運作、也方便分享連結。

const HTTP_METHODS = ["get", "post", "put", "patch", "delete"];

const METHOD_BADGE_CLASS = {
  get: "reference-method-get",
  post: "reference-method-post",
  put: "reference-method-put",
  patch: "reference-method-patch",
  delete: "reference-method-delete",
};

let SPEC = null;
let GETTING_STARTED_HTML = "";

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function methodBadge(method) {
  const cls = METHOD_BADGE_CLASS[method] || "";
  return `<span class="badge ${cls}">${method.toUpperCase()}</span>`;
}

// -----------------------------------------------------------------------------
// $ref 解析與 schema 走訪
// -----------------------------------------------------------------------------

function resolveRef(ref) {
  if (!ref) return null;
  const parts = ref.replace(/^#\//, "").split("/");
  let node = SPEC;
  for (const part of parts) {
    if (node == null) return null;
    node = node[part];
  }
  return node;
}

// 只展開最外層的 $ref；巢狀屬性各自的 $ref 由呼叫端在走訪時再展開一次，
// 避免這裡遞迴展開整棵樹（有些 schema 之間會互相參照，一次展開到底
// 容易變成無限遞迴）。
//
// 同時把 `allOf` 展開合併成單一個 object schema——契約裡 24 個 schema
// （例如 ReservationDetail = Reservation + 額外欄位）用 allOf 表示組合，
// 不展開的話 schemaRows／exampleFromSchema 只看得到最外層（沒有
// properties），畫出來的表格與範例會是空的，卻不會有任何錯誤訊息。
function resolveSchema(schema) {
  if (!schema) return null;
  if (schema.$ref) return resolveSchema(resolveRef(schema.$ref));
  if (Array.isArray(schema.allOf)) {
    const merged = { type: "object", properties: {}, required: [] };
    for (const sub of schema.allOf) {
      const flat = resolveSchema(sub);
      if (!flat) continue;
      Object.assign(merged.properties, flat.properties || {});
      merged.required = merged.required.concat(flat.required || []);
    }
    merged.description_html = schema.description_html || merged.description_html;
    if (schema.example !== undefined) merged.example = schema.example;
    return merged;
  }
  if (Array.isArray(schema.oneOf) && schema.oneOf.length) {
    // 文件呈現只挑第一個變體代表整個 schema——比起窮舉每一種可能，
    // 這裡優先求「範例看得懂」。契約目前只有 2 處用 oneOf。
    const first = resolveSchema(schema.oneOf[0]);
    return first && { ...first, description_html: schema.description_html || first.description_html };
  }
  return schema;
}

function resolveParam(param) {
  return param && param.$ref ? resolveRef(param.$ref) : param;
}

// 一個路徑的參數可以宣告在 path item 層級（跨該路徑下所有方法共用，
// 例如 `/scim/v2/Users/{id}` 的 `id` 只在最外層宣告一次，GET／PATCH／
// DELETE 都不會重複列），也可以宣告在 operation 層級。少看 path item
// 層級會讓這些共用參數（通常正是 `{id}` 這種路徑參數）整個消失——
// 連 curl 範例裡的 `{id}` 都不會被替換掉。同名同位置時 operation 層級蓋掉
// path item 層級，這是 OpenAPI 規格本身定義的覆寫規則。
function operationParameters(path, op) {
  const pathItem = SPEC.paths[path] || {};
  const pathLevel = (pathItem.parameters || []).map(resolveParam).filter(Boolean);
  const opLevel = (op.parameters || []).map(resolveParam).filter(Boolean);
  const keyOf = (p) => `${p.in}:${p.name}`;
  const overridden = new Set(opLevel.map(keyOf));
  return [...pathLevel.filter((p) => !overridden.has(keyOf(p))), ...opLevel];
}

function describeType(rawSchema) {
  const schema = resolveSchema(rawSchema);
  if (!schema) return "any";
  if (schema.type === "array") return describeType(schema.items) + "[]";
  if (schema.enum) return "enum(" + schema.enum.map((v) => JSON.stringify(v)).join(" | ") + ")";
  if (schema.type === "object" || schema.properties) return "object";
  if (schema.format) return `${schema.type}(${schema.format})`;
  return schema.type || "any";
}

// 把一個 schema 遞迴攤平成表格列（name／type／required／description）。
// 巢狀 object／array-of-object 的屬性用 `parent.child` 的方式命名，
// 這樣一張表就能看完整個結構，不需要點進去展開。
function schemaRows(rawSchema, prefix) {
  const schema = resolveSchema(rawSchema);
  const rows = [];
  if (!schema) return rows;
  if (schema.type === "array") {
    return schemaRows(schema.items, (prefix || "") + "[]");
  }
  const props = schema.properties || {};
  const required = new Set(schema.required || []);
  for (const [name, rawPropSchema] of Object.entries(props)) {
    const propSchema = resolveSchema(rawPropSchema);
    const fullName = prefix ? `${prefix}.${name}` : name;
    rows.push({
      name: fullName,
      type: describeType(rawPropSchema),
      required: required.has(name),
      descriptionHtml: (propSchema && propSchema.description_html) || "",
    });
    if (propSchema && (propSchema.type === "object" || propSchema.properties)) {
      rows.push(...schemaRows(propSchema, fullName));
    } else if (propSchema && propSchema.type === "array" && propSchema.items) {
      const itemSchema = resolveSchema(propSchema.items);
      if (itemSchema && (itemSchema.type === "object" || itemSchema.properties)) {
        rows.push(...schemaRows(itemSchema, fullName + "[]"));
      }
    }
  }
  return rows;
}

// 依 schema 的型別生成一個範例值：schema 本身若帶 example／enum 就直接用，
// 否則生出一個型別對的骨架（字串填 "string"、數字填 0…）。深度上限防止
// 互相參照的 schema 造成無限遞迴。
function exampleFromSchema(rawSchema, depth) {
  depth = depth || 0;
  if (depth > 6) return null;
  const schema = resolveSchema(rawSchema);
  if (!schema) return null;
  if (schema.example !== undefined) return schema.example;
  if (Array.isArray(schema.examples) && schema.examples.length) return schema.examples[0];
  if (Array.isArray(schema.enum) && schema.enum.length) return schema.enum[0];
  if (schema.type === "array") {
    const item = exampleFromSchema(schema.items, depth + 1);
    return item === null ? [] : [item];
  }
  if (schema.type === "object" || schema.properties) {
    const out = {};
    for (const [name, propSchema] of Object.entries(schema.properties || {})) {
      out[name] = exampleFromSchema(propSchema, depth + 1);
    }
    return out;
  }
  switch (schema.type) {
    case "string":
      if (schema.format === "date-time") return "2026-08-05T09:00:00Z";
      if (schema.format === "date") return "2026-08-05";
      if (schema.format === "uuid") return "00000000-0000-4000-8000-000000000000";
      return "string";
    case "integer":
    case "number":
      return 0;
    case "boolean":
      return true;
    default:
      return null;
  }
}

function jsonRequestSchema(requestBodyOrResponse) {
  const content = requestBodyOrResponse && requestBodyOrResponse.content;
  return content && content["application/json"] && content["application/json"].schema;
}

function findSuccessResponse(op) {
  const responses = op.responses || {};
  for (const code of Object.keys(responses)) {
    if (/^2\d\d$/.test(code)) {
      const raw = responses[code];
      return { code, response: raw && raw.$ref ? resolveRef(raw.$ref) : raw };
    }
  }
  return null;
}

// -----------------------------------------------------------------------------
// curl 範例
// -----------------------------------------------------------------------------

// 這份契約有兩種安全機制：一般端點用 `bearerAuth`（需要 X-Tenant-ID 才能
// 決定租戶範圍），SCIM 用單獨的 `scimToken`（依身分來源設定，不帶
// X-Tenant-ID——SCIM 的 10 支端點也確實沒有把它宣告成參數）。用哪個
// scheme 由 operation 自己的 `security`（未宣告則退回契約層級的預設）
// 決定，不是每個端點都一樣。
function securitySchemeNames(op) {
  const security = Array.isArray(op.security) ? op.security : SPEC.security || [];
  const names = new Set();
  for (const requirement of security) {
    for (const name of Object.keys(requirement)) names.add(name);
  }
  return names;
}

function buildCurl(method, path, op) {
  const params = operationParameters(path, op);
  let urlPath = path;
  const queryParts = [];
  for (const p of params) {
    const example = exampleFromSchema(p.schema);
    const value = example === null || example === undefined ? "..." : example;
    if (p.in === "path") {
      urlPath = urlPath.replace(`{${p.name}}`, encodeURIComponent(String(value)));
    } else if (p.in === "query" && p.required) {
      queryParts.push(`${p.name}=${encodeURIComponent(String(value))}`);
    }
  }
  const query = queryParts.length ? "?" + queryParts.join("&") : "";

  const segments = [`curl -X ${method.toUpperCase()}`, `"{baseUrl}/api/v1${urlPath}${query}"`];
  const schemes = securitySchemeNames(op);
  if (schemes.size > 0) {
    segments.push('-H "Authorization: Bearer <token>"');
    if (schemes.has("bearerAuth")) {
      segments.push('-H "X-Tenant-ID: <tenant-uuid>"');
    }
  }
  // Authorization／X-Tenant-ID 已經在上面通用加過一次——很多 operation 把
  // X-Tenant-ID 也宣告成自己的 header 參數（給契約文件用），這裡跳過它們
  // 避免同一個 header 在範例裡出現兩次。
  const genericHeaders = new Set(["authorization", "x-tenant-id"]);
  for (const p of params.filter((p) => p.in === "header" && !genericHeaders.has(p.name.toLowerCase()))) {
    segments.push(`-H "${p.name}: <${p.name}>"`);
  }
  const requestBodySchema = jsonRequestSchema(op.requestBody);
  if (requestBodySchema) {
    segments.push('-H "Content-Type: application/json"');
    const example = exampleFromSchema(requestBodySchema);
    segments.push(`-d '${JSON.stringify(example, null, 2)}'`);
  }
  return segments.join(" \\\n  ");
}

// -----------------------------------------------------------------------------
// 測試設定（Base URL／Token／X-Tenant-ID）—— 存在 localStorage，
// 只會用在「送出測試請求」這個功能，不會出現在任何其他地方（不上報、
// 不打第三方）。三個輸入框在 index.html 的側欄裡，跨分頁共用同一組值，
// 這樣切換不同 operation 測試時不用重填。
// -----------------------------------------------------------------------------

const SETTINGS_STORAGE_KEY = "fmsref.settings";

function loadSettings() {
  try {
    return JSON.parse(localStorage.getItem(SETTINGS_STORAGE_KEY)) || {};
  } catch {
    return {};
  }
}

function saveSettings(settings) {
  localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify(settings));
}

function currentSettings() {
  return {
    baseUrl: document.getElementById("settings-base-url").value.trim().replace(/\/+$/, ""),
    token: document.getElementById("settings-token").value.trim(),
    tenantId: document.getElementById("settings-tenant-id").value.trim(),
  };
}

function setupSettingsPanel() {
  const saved = loadSettings();
  const baseUrlInput = document.getElementById("settings-base-url");
  const tokenInput = document.getElementById("settings-token");
  const tenantIdInput = document.getElementById("settings-tenant-id");
  baseUrlInput.value = saved.baseUrl || "";
  tokenInput.value = saved.token || "";
  tenantIdInput.value = saved.tenantId || "";
  for (const el of [baseUrlInput, tokenInput, tenantIdInput]) {
    el.addEventListener("input", () => saveSettings(currentSettings()));
  }
}

// -----------------------------------------------------------------------------
// 互動測試 —— 真的送出 fetch()，不是產生範例。
//
// 表單欄位都預填 exampleFromSchema() 算出來的值（跟 curl／範例回應共用
// 同一套邏輯，不重寫第二份），使用者可以直接改。CORS 是瀏覽器強制的
// 安全機制，這裡繞不過——失敗時明確告訴使用者可能是目標伺服器的
// CORS_ALLOWED_ORIGINS 沒有包含這個網站的來源，而不是丟一個看不懂的
// "Failed to fetch"。
// -----------------------------------------------------------------------------

function renderTryItOut(method, path, op) {
  const params = operationParameters(path, op);
  const genericHeaders = new Set(["authorization", "x-tenant-id"]);
  const pathParams = params.filter((p) => p.in === "path");
  const queryParams = params.filter((p) => p.in === "query");
  const headerParams = params.filter(
    (p) => p.in === "header" && !genericHeaders.has(p.name.toLowerCase())
  );
  const requestBodySchema = jsonRequestSchema(op.requestBody);
  const bodyExample = requestBodySchema ? exampleFromSchema(requestBodySchema) : null;

  const paramRow = (p, inKind) => {
    const example = exampleFromSchema(p.schema);
    const value = example === null || example === undefined ? "" : String(example);
    return `
      <div class="reference-tryit-row">
        <span class="reference-tryit-param-name"><code>${escapeHtml(p.name)}</code>${p.required ? " *" : ""}</span>
        <input class="form-control form-control-sm reference-tryit-param"
               data-in="${inKind}" data-name="${escapeHtml(p.name)}" value="${escapeHtml(value)}">
      </div>`;
  };

  return `
    <div class="reference-tryit">
      ${pathParams.length ? `<div class="reference-tryit-group-label">路徑參數</div>${pathParams.map((p) => paramRow(p, "path")).join("")}` : ""}
      ${queryParams.length ? `<div class="reference-tryit-group-label">查詢參數（空白＝不帶）</div>${queryParams.map((p) => paramRow(p, "query")).join("")}` : ""}
      ${headerParams.length ? `<div class="reference-tryit-group-label">其他標頭（空白＝不帶）</div>${headerParams.map((p) => paramRow(p, "header")).join("")}` : ""}
      ${
        requestBodySchema
          ? `<div class="reference-tryit-group-label">Request Body（可編輯 JSON）</div>
             <textarea class="reference-tryit-body" id="tryit-body">${escapeHtml(JSON.stringify(bodyExample, null, 2))}</textarea>`
          : ""
      }
      <button type="button" id="tryit-send" class="btn btn-primary btn-sm mt-2">送出請求</button>
      <div id="tryit-response" class="reference-tryit-response"></div>
    </div>`;
}

function wireTryItOut(method, path, op) {
  const btn = document.getElementById("tryit-send");
  if (!btn) return;
  btn.addEventListener("click", () => sendTestRequest(method, path, op));
}

async function sendTestRequest(method, path, op) {
  const responseEl = document.getElementById("tryit-response");
  const settings = currentSettings();
  if (!settings.baseUrl) {
    responseEl.innerHTML = `<div class="alert alert-warning">請先在左側「測試設定」填 Base URL。</div>`;
    return;
  }

  let urlPath = path;
  const queryParts = [];
  document.querySelectorAll(".reference-tryit-param").forEach((input) => {
    const inKind = input.dataset.in;
    const name = input.dataset.name;
    const value = input.value;
    if (inKind === "path") {
      urlPath = urlPath.replace(`{${name}}`, encodeURIComponent(value));
    } else if (inKind === "query" && value !== "") {
      queryParts.push(`${encodeURIComponent(name)}=${encodeURIComponent(value)}`);
    }
  });
  const query = queryParts.length ? "?" + queryParts.join("&") : "";
  const url = `${settings.baseUrl}/api/v1${urlPath}${query}`;

  const headers = {};
  const schemes = securitySchemeNames(op);
  if (schemes.size > 0 && settings.token) {
    headers["Authorization"] = `Bearer ${settings.token}`;
  }
  if (schemes.has("bearerAuth") && settings.tenantId) {
    headers["X-Tenant-ID"] = settings.tenantId;
  }
  document.querySelectorAll(".reference-tryit-param[data-in='header']").forEach((input) => {
    if (input.value !== "") headers[input.dataset.name] = input.value;
  });

  let body;
  const bodyTextarea = document.getElementById("tryit-body");
  if (bodyTextarea) {
    body = bodyTextarea.value.trim() ? bodyTextarea.value : undefined;
    if (body) {
      try {
        JSON.parse(body);
      } catch (e) {
        responseEl.innerHTML = `<div class="alert alert-danger">Request Body 不是合法的 JSON：${escapeHtml(e.message)}</div>`;
        return;
      }
      headers["Content-Type"] = "application/json";
    }
  }

  responseEl.innerHTML = `<div class="text-secondary small">送出中…</div>`;
  const startedAt = performance.now();
  try {
    const res = await fetch(url, { method: method.toUpperCase(), headers, body });
    const elapsed = Math.round(performance.now() - startedAt);
    const text = await res.text();
    let pretty = text;
    try {
      pretty = JSON.stringify(JSON.parse(text), null, 2);
    } catch {
      // 不是 JSON，原樣顯示。
    }
    const headerLines = [];
    res.headers.forEach((v, k) => headerLines.push(`${k}: ${v}`));
    const statusClass = res.ok ? "reference-tryit-status-ok" : "reference-tryit-status-err";
    responseEl.innerHTML = `
      <div class="${statusClass}">${res.status} ${escapeHtml(res.statusText)} · ${elapsed}ms</div>
      <pre><code>${escapeHtml(headerLines.join("\n"))}\n\n${escapeHtml(pretty)}</code></pre>`;
  } catch (err) {
    responseEl.innerHTML = `
      <div class="alert alert-danger">
        請求失敗：${escapeHtml(err.message)}<br>
        可能原因：Base URL 打錯、目標伺服器沒開、或目標伺服器的
        <code>CORS_ALLOWED_ORIGINS</code> 沒有包含這個網站的來源
        （<code>${escapeHtml(location.origin)}</code>）。
      </div>`;
  }
}

// -----------------------------------------------------------------------------
// Sidebar
// -----------------------------------------------------------------------------

function collectOperationsForTag(tagName) {
  const results = [];
  for (const [path, methods] of Object.entries(SPEC.paths || {})) {
    for (const method of HTTP_METHODS) {
      const op = methods[method];
      if (op && Array.isArray(op.tags) && op.tags.includes(tagName)) {
        results.push({ method, path, op });
      }
    }
  }
  return results;
}

function renderSidebar() {
  const container = document.getElementById("sidebar-groups");
  const groups = [];
  for (const tag of SPEC.tags || []) {
    const ops = collectOperationsForTag(tag.name);
    if (!ops.length) continue;
    const links = ops
      .map(({ method, path, op }) => {
        const operationId = op.operationId || `${method}_${path}`;
        const searchText = `${path} ${op.summary || ""} ${operationId}`.toLowerCase();
        return `
          <a class="reference-nav-link"
             href="#/operations/${encodeURIComponent(operationId)}"
             data-search-text="${escapeHtml(searchText)}">
            <span class="reference-method-badge">${methodBadge(method)}</span>
            <span class="reference-path">${escapeHtml(path)}</span>
          </a>`;
      })
      .join("");
    groups.push(`
      <div class="reference-sidebar-group">
        <div class="reference-sidebar-group-title" title="${escapeHtml(tag.description || "")}">
          ${escapeHtml(tag.name)}
        </div>
        ${links}
      </div>`);
  }
  container.innerHTML = groups.join("");
}

function updateActiveSidebarLink() {
  const hash = location.hash || "#/getting-started";
  document.querySelectorAll(".reference-nav-link").forEach((link) => {
    link.classList.toggle("active", link.getAttribute("href") === hash);
  });
}

function setupSearch() {
  const input = document.getElementById("search-input");
  input.addEventListener("input", () => {
    const query = input.value.trim().toLowerCase();
    document.querySelectorAll(".reference-nav-link").forEach((link) => {
      const matches = query === "" || link.dataset.searchText.includes(query);
      link.classList.toggle("reference-hidden", !matches);
    });
    document.querySelectorAll(".reference-sidebar-group").forEach((group) => {
      const anyVisible = group.querySelector(".reference-nav-link:not(.reference-hidden)");
      group.classList.toggle("reference-hidden", !anyVisible);
    });
  });
}

// -----------------------------------------------------------------------------
// 右側面板
// -----------------------------------------------------------------------------

function renderGettingStarted() {
  document.getElementById("main-content").innerHTML = `
    <div class="container-xl">
      <div class="card">
        <div class="card-body reference-getting-started">
          ${GETTING_STARTED_HTML}
        </div>
      </div>
    </div>`;
}

function findOperation(operationId) {
  for (const [path, methods] of Object.entries(SPEC.paths || {})) {
    for (const method of HTTP_METHODS) {
      const op = methods[method];
      if (op && (op.operationId === operationId || `${method}_${path}` === operationId)) {
        return { method, path, op };
      }
    }
  }
  return null;
}

function renderParamsTable(params) {
  const resolved = (params || []).map(resolveParam).filter(Boolean);
  if (!resolved.length) return "";
  const rows = resolved
    .map((p) => {
      const descHtml = p.description_html || "";
      return `
        <tr>
          <td><code>${escapeHtml(p.name)}</code></td>
          <td><span class="badge bg-secondary-lt">${escapeHtml(p.in)}</span></td>
          <td>${escapeHtml(describeType(p.schema))}</td>
          <td>${p.required ? '<span class="badge bg-red-lt">必填</span>' : ""}</td>
          <td>${descHtml}</td>
        </tr>`;
    })
    .join("");
  return `
    <h3 class="reference-section-title">參數</h3>
    <div class="table-responsive">
      <table class="table table-vcenter card-table">
        <thead><tr><th>名稱</th><th>位置</th><th>型別</th><th>必填</th><th>說明</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>`;
}

function renderSchemaTable(title, rawSchema) {
  const rows = schemaRows(rawSchema, "");
  if (!rows.length) return "";
  const body = rows
    .map(
      (r) => `
        <tr>
          <td><code>${escapeHtml(r.name)}</code></td>
          <td>${escapeHtml(r.type)}</td>
          <td>${r.required ? '<span class="badge bg-red-lt">必填</span>' : ""}</td>
          <td>${r.descriptionHtml}</td>
        </tr>`
    )
    .join("");
  return `
    <h3 class="reference-section-title">${escapeHtml(title)}</h3>
    <div class="table-responsive">
      <table class="table table-vcenter card-table">
        <thead><tr><th>欄位</th><th>型別</th><th>必填</th><th>說明</th></tr></thead>
        <tbody>${body}</tbody>
      </table>
    </div>`;
}

function renderExampleBlock(title, value) {
  if (value === null || value === undefined) return "";
  const text = typeof value === "string" ? value : JSON.stringify(value, null, 2);
  return `
    <h3 class="reference-section-title">${escapeHtml(title)}</h3>
    <div class="reference-example"><pre><code>${escapeHtml(text)}</code></pre></div>`;
}

function renderResponsesTable(op) {
  const responses = op.responses || {};
  const rows = Object.entries(responses)
    .map(([code, raw]) => {
      const resolved = raw && raw.$ref ? resolveRef(raw.$ref) : raw;
      const descHtml = (resolved && resolved.description_html) || escapeHtml((resolved && resolved.description) || "");
      return `<tr><td><code>${escapeHtml(code)}</code></td><td>${descHtml}</td></tr>`;
    })
    .join("");
  if (!rows) return "";
  return `
    <h3 class="reference-section-title">回應狀態碼</h3>
    <div class="table-responsive">
      <table class="table table-vcenter card-table">
        <thead><tr><th>狀態碼</th><th>說明</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    </div>`;
}

function renderOperation(operationId) {
  const found = findOperation(operationId);
  const main = document.getElementById("main-content");
  if (!found) {
    main.innerHTML = `<div class="container-xl"><div class="alert alert-danger">找不到 operation：${escapeHtml(operationId)}</div></div>`;
    return;
  }
  const { method, path, op } = found;
  const requestBodySchema = jsonRequestSchema(op.requestBody);
  const successResponse = findSuccessResponse(op);
  const responseSchema = successResponse && jsonRequestSchema(successResponse.response);

  main.innerHTML = `
    <div class="container-xl">
      <div class="reference-op-header">
        ${methodBadge(method)}
        <code>${escapeHtml(path)}</code>
      </div>
      <h2>${escapeHtml(op.summary || op.operationId || "")}</h2>
      <div class="reference-op-summary">${op.description_html || ""}</div>

      ${renderParamsTable(operationParameters(path, op))}
      ${requestBodySchema ? renderSchemaTable("Request Body", requestBodySchema) : ""}
      ${requestBodySchema ? renderExampleBlock("範例 Request Body", exampleFromSchema(requestBodySchema)) : ""}

      <h3 class="reference-section-title">範例 curl</h3>
      <div class="reference-example"><pre><code>${escapeHtml(buildCurl(method, path, op))}</code></pre></div>

      ${renderResponsesTable(op)}
      ${responseSchema ? renderSchemaTable(`範例回應（${successResponse.code}）欄位`, responseSchema) : ""}
      ${responseSchema ? renderExampleBlock(`範例回應（${successResponse.code}）`, exampleFromSchema(responseSchema)) : ""}

      <h3 class="reference-section-title">互動測試</h3>
      ${renderTryItOut(method, path, op)}
    </div>`;

  wireTryItOut(method, path, op);
}

// -----------------------------------------------------------------------------
// Routing
// -----------------------------------------------------------------------------

function router() {
  const hash = location.hash || "#/getting-started";
  const match = hash.match(/^#\/operations\/(.+)$/);
  if (match) {
    renderOperation(decodeURIComponent(match[1]));
  } else {
    renderGettingStarted();
  }
  updateActiveSidebarLink();
}

async function init() {
  const main = document.getElementById("main-content");
  try {
    const [specResp, gettingStartedResp] = await Promise.all([
      fetch("data/openapi.json"),
      fetch("data/getting-started.html"),
    ]);
    if (!specResp.ok || !gettingStartedResp.ok) {
      throw new Error("data/ 底下的檔案抓不到——記得先跑 build.py");
    }
    SPEC = await specResp.json();
    GETTING_STARTED_HTML = await gettingStartedResp.text();
  } catch (err) {
    main.innerHTML = `<div class="container-xl"><div class="alert alert-danger">
      載入失敗：${escapeHtml(err.message)}。<br>
      本機預覽請先在 repo 根目錄跑 <code>python3 api/reference-site/build.py</code>，
      再從 <code>api/reference-site/</code> 開一個本機伺服器（例如
      <code>python3 -m http.server</code>），不要直接用 <code>file://</code> 開
      index.html（瀏覽器會擋 fetch 讀本機檔案）。
    </div></div>`;
    return;
  }

  renderVersionInfo();
  renderSidebar();
  setupSearch();
  setupSettingsPanel();
  window.addEventListener("hashchange", router);
  router();
}

// 版本沿用 openapi.yaml 自己宣告的 info.version（唯一權威）；日期是
// build.py 產生這份 data/ 的日期（見該檔案的說明），不是寫死的字串——
// 每次重新建置都會自動反映真正的發布時間。
function renderVersionInfo() {
  const el = document.getElementById("version-info");
  if (!el || !SPEC.info) return;
  const version = SPEC.info.version;
  const generatedAt = SPEC.info["x-generated-at"];
  el.textContent = [version && `v${version}`, generatedAt && `發佈於 ${generatedAt}`]
    .filter(Boolean)
    .join(" · ");
}

init();
