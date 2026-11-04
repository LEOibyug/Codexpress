'use strict';

(() => {
  const $ = (id) => document.getElementById(id);
  const tabs = ['overview', 'requests', 'cache'];
  const titles = { overview: '概览与性能', requests: '请求日志', cache: '原文缓存' };
  const subtitles = {
    overview: '按本地日历聚合整个筛选窗口，不受日志分页影响。',
    requests: '每条记录对应一次请求尝试；重试和切换来源分别记录。',
    cache: '查看已保留的对话、工具调用与历史快照。',
  };
  const statusNames = { success: '成功', failed: '失败', interrupted: '已中断', active: '进行中', connecting: '连接中', streaming: '接收中' };
  const state = {
    tab: 'overview', filter: null, epoch: 0, busy: false,
    controllers: new Map(), records: [], cacheRecords: [], next: null, pages: 0,
    total: 0, active: 0, legacy: 0, unfiltered: false, attempt: '', detail: null,
    lastFocus: null,
  };
  const numberFormat = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 });
  const isNumber = (value) => typeof value === 'number' && Number.isFinite(value);
  const n = (value) => isNumber(value) ? numberFormat.format(value) : '未知';
  const usd = (value) => isNumber(value) ? '$' + value.toLocaleString('en-US', { minimumFractionDigits: 4, maximumFractionDigits: 6 }) : '未知';
  const ms = (value) => isNumber(value) ? (value >= 1000 ? n(value / 1000) + ' s' : n(value) + ' ms') : '未知';
  const pct = (a, b) => isNumber(a) && isNumber(b) && b > 0 ? n(a / b * 100) + '%' : '未知';
  const stamp = (value) => isNumber(value) ? new Date(value * 1000).toLocaleString('zh-CN', { hour12: false }) : '未记录';
  const json = (value) => JSON.stringify(value, null, 2) ?? '未记录';
  function node(tag, className, text) {
    const result = document.createElement(tag);
    if (className) result.className = className;
    if (text !== undefined) result.textContent = text;
    return result;
  }
  function button(text, action, className = 'secondary') {
    const result = node('button', className, text);
    result.type = 'button';
    result.addEventListener('click', action);
    return result;
  }
  function message(text, error = false) {
    $('feedback').textContent = text;
    $('feedback').className = error ? 'error' : '';
  }
  function dateValue(date) {
    return [date.getFullYear(), String(date.getMonth() + 1).padStart(2, '0'), String(date.getDate()).padStart(2, '0')].join('-');
  }
  function localDate(value) {
    const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
    if (!match) throw new Error('请选择有效日期。');
    const result = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
    if (dateValue(result) !== value || result.getTime() < 0) throw new Error('请选择 1970 年之后的有效日期。');
    return result;
  }
  function quickRange(days) {
    const end = new Date();
    const start = new Date(end.getFullYear(), end.getMonth(), end.getDate());
    start.setDate(start.getDate() - days + 1);
    $('filter-from').value = dateValue(start);
    $('filter-to').value = dateValue(end);
    document.querySelectorAll('[data-days]').forEach((item) => item.setAttribute('aria-pressed', String(Number(item.dataset.days) === days)));
  }
  function readFilters() {
    const start = localDate($('filter-from').value);
    const end = localDate($('filter-to').value);
    if (end < start) throw new Error('结束日期不能早于开始日期。');
    const days = [];
    const cursor = new Date(start);
    while (cursor <= end && days.length <= 366) {
      days.push(Math.floor(cursor.getTime() / 1000));
      cursor.setDate(cursor.getDate() + 1);
    }
    if (days.length > 366) throw new Error('每次最多选择 366 个本地日历日。请缩小范围；可另选任意历史日期。');
    days.push(Math.floor(cursor.getTime() / 1000));
    const result = { from: days[0], to: days[days.length - 1], day_starts: days };
    for (const [field, id] of [['source_id', 'source'], ['model', 'model'], ['ingress', 'ingress'], ['status', 'status'], ['search', 'search']]) {
      const value = $('filter-' + id).value.trim();
      if (value) result[field] = value;
    }
    return result;
  }
  function query(kind) {
    if (kind === 'cache' && state.unfiltered) return {};
    if (kind === 'cache' && state.attempt) return { attempt_id: state.attempt };
    const result = { ...state.filter };
    if (kind !== 'analytics') delete result.day_starts;
    if (kind === 'cache') {
      delete result.status;
      delete result.search;
    }
    return result;
  }
  function abort(scope) {
    state.controllers.get(scope)?.abort();
    state.controllers.delete(scope);
  }
  function controller(scope) {
    abort(scope);
    const result = new AbortController();
    state.controllers.set(scope, result);
    return result;
  }
  function cancelPending() {
    state.epoch++;
    for (const value of state.controllers.values()) value.abort();
    state.controllers.clear();
    state.busy = false;
    $('refresh').disabled = false;
    $('requests-more').disabled = false;
    $('cache-more').disabled = false;
  }
  async function api(path, params, signal) {
    const url = new URL('/dashboard/api/' + path, window.location.origin);
    for (const [key, value] of Object.entries(params || {})) {
      if (value !== undefined && value !== null) url.searchParams.set(key, Array.isArray(value) ? JSON.stringify(value) : String(value));
    }
    const response = await fetch(url.pathname + url.search, { cache: 'no-store', signal });
    const data = await response.json();
    if (!response.ok) throw new Error('读取失败（HTTP ' + response.status + '）：' + data.error.message);
    return data;
  }
  function report(error, destination) {
    if (error.name === 'AbortError') return;
    if (destination && destination.isConnected) destination.textContent = error.message || '读取失败，请重试。';
    else message(error.message || '读取失败，请重试。', true);
  }
  function fillOptions(id, items, placeholder, valueOf, labelOf) {
    const select = $(id);
    const selected = select.value;
    select.replaceChildren(new Option(placeholder, ''));
    const seen = new Set();
    for (const item of items || []) {
      const value = String(valueOf(item));
      if (seen.has(value)) continue;
      seen.add(value);
      select.add(new Option(labelOf(item), value));
    }
    if (selected && !seen.has(selected)) select.add(new Option(selected + '（当前筛选）', selected));
    select.value = selected;
  }
  function renderOptions(options) {
    if (!options) return;
    fillOptions('filter-source', options.sources, '全部来源', (x) => x.id, (x) => (x.name || x.id) + (x.name && x.name !== x.id ? ' · ' + x.id : ''));
    fillOptions('filter-model', options.models, '全部模型', (x) => x, (x) => x || '未记录模型');
    fillOptions('filter-ingress', options.ingresses, '全部入口', (x) => x, (x) => ({ native: '本机', ssh: 'SSH', chatbot: '聊天机器人', image: '图像', unknown: '未知' })[x] || x);
  }
  function table(headers, rows, empty = '当前筛选范围内没有记录。', rowActions = []) {
    if (!rows.length) return node('p', 'empty', empty);
    const result = node('table');
    const head = node('thead');
    const headerRow = node('tr');
    for (const label of headers) {
      const cell = node('th', '', label);
      cell.scope = 'col';
      headerRow.append(cell);
    }
    head.append(headerRow);
    const body = node('tbody');
    for (const values of rows) {
      const row = node('tr');
      for (const value of values) {
        const cell = node('td');
        if (value instanceof Node) cell.append(value);
        else cell.textContent = value ?? '未知';
        row.append(cell);
      }
      const action = rowActions[body.childElementCount];
      if (action) {
        row.addEventListener('click', (event) => {
          if (!event.target.closest('button, a, input, select')) action();
        });
        row.style.cursor = 'pointer';
      }
      body.append(row);
    }
    result.append(head, body);
    return result;
  }
  function totals(groups) {
    const result = {};
    for (const group of groups) {
      for (const [field, value] of Object.entries(group)) {
        if (isNumber(value)) result[field] = (result[field] ?? 0) + value;
      }
    }
    if (!groups.length) {
      for (const field of ['requests', 'successes', 'active_requests', 'input_tokens', 'output_tokens', 'total_tokens', 'cached_input_tokens', 'estimated_requests', 'priced_requests', 'estimated_cost_requests', 'upstream_priced_requests', 'unpriced_requests', 'unobserved_requests', 'partial_cost_requests']) result[field] = 0;
    }
    return result;
  }
  function kpi(label, value, note) {
    const card = node('div', 'kpi');
    card.append(node('p', 'kpi-label', label), node('p', 'kpi-value', value), node('p', 'kpi-note', note));
    return card;
  }
  function renderOverview(data, status) {
    renderOptions(data.options);
    const groups = data.groups || [];
    const t = totals(groups);
    const p = data.performance || {};
    const ended = (t.requests || 0) - (t.active_requests || 0);
    const tps = t.throughput_latency_ms > 0 ? t.throughput_output_tokens / (t.throughput_latency_ms / 1000) : null;
    $('kpis').replaceChildren(
      kpi('请求总数', n(t.requests), '整个筛选窗口 · 按尝试计数'),
      kpi('成功率', pct(t.successes, ended), n(t.successes) + ' 成功 / ' + n(ended) + ' 已结束'),
      kpi('进行中', n(t.active_requests), '来自网关当前活动快照'),
      kpi('已知费用 · USD', usd(t.total_usd), '上游扣费 ' + n(t.upstream_priced_requests) + ' 条 · 参考价 ' + n(Math.max(0, (t.priced_requests || 0) - (t.upstream_priced_requests || 0))) + ' 条'),
      kpi('总 token', n(t.total_tokens), '输入 + 输出；估算样本 ' + n(t.estimated_requests) + ' 条'),
      kpi('输入 token', n(t.input_tokens), '含已报告缓存输入；部分记录可能缺失'),
      kpi('输出 token', n(t.output_tokens), '已知输出；不重复累加推理 token'),
      kpi('缓存输入 token', n(t.cached_input_tokens), '记录的缓存量；未报告不等于未命中'),
      kpi('推理 token', n(t.reasoning_output_tokens), '仅统计上游明确报告的推理量'),
      kpi('加权输出 TPS', n(tps), '有效输出 token / 对应成功请求总耗时（秒）'),
      kpi('首 token · P50 / P95', ms(p.p50_first_token_ms) + ' / ' + ms(p.p95_first_token_ms), 'TTFT · 仅有首 token 时序的已结束样本'),
      kpi('完整耗时 · P50 / P95', ms(p.p50_latency_ms) + ' / ' + ms(p.p95_latency_ms), '完整请求耗时 · 非仅生成阶段'),
      kpi('已报告缓存命中率', pct(t.cached_reported_tokens, t.cached_reported_input_tokens), n(t.cached_reported_tokens) + ' / ' + n(t.cached_reported_input_tokens) + ' 可用输入 token'),
      kpi('已知缓存节省 · USD', usd(t.cache_savings_usd), '按已知价格计算；未知不补零'),
      kpi('费用数据覆盖', n(t.priced_requests) + ' / ' + n(t.requests), '已定价 / 总请求；未知价格 ' + n(t.unpriced_requests) + ' · 未观测 ' + n(t.unobserved_requests) + ' · 部分 ' + n(t.partial_cost_requests)),
    );
    $('groups').replaceChildren(table(
      ['来源', '模型', '请求', '成功率', '进行中', '输入', '输出', '缓存', '推理', 'USD（已知）', '实际 / 参考', '输出 TPS'],
      groups.map((g) => [g.source_name || g.source_id || '未记录', g.model || '未记录', n(g.requests), pct(g.successes, g.requests - g.active_requests), n(g.active_requests), n(g.input_tokens), n(g.output_tokens), n(g.cached_input_tokens), n(g.reasoning_output_tokens), usd(g.total_usd), n(g.upstream_priced_requests) + ' / ' + n(Math.max(0, (g.priced_requests || 0) - (g.upstream_priced_requests || 0))), n(g.throughput_latency_ms > 0 ? g.throughput_output_tokens / (g.throughput_latency_ms / 1000) : null)]),
    ));
    renderDaily(data.daily || []);
    const sources = Array.isArray(status.sources) ? status.sources : [];
    $('gateway-status').replaceChildren(
      node('span', '', '本地网关已连接'),
      node('span', '', '配置来源 ' + sources.length + ' 个'),
      node('span', '', '数据更新 ' + stamp(data.fetched_at)),
    );
    $('status-details').replaceChildren(rawDetails('本地状态、历史与对账信息（原始字段）', status, true));
  }
  function renderDaily(daily) {
    $('daily-summary').textContent = daily.length + ' 个本地日历日';
    $('daily-table').replaceChildren(table(['日期', '请求', '成功', '失败', '输入', '输出', '缓存', '总 token', 'USD（已知）'], daily.map((d) => [dateValue(new Date(d.start_at * 1000)), n(d.requests), n(d.successes), n(d.failures), n(d.input_tokens), n(d.output_tokens), n(d.cached_input_tokens), n(d.total_tokens), usd(d.total_usd)])));
    const host = $('daily-chart');
    host.replaceChildren();
    if (!daily.length) { host.append(node('p', 'empty', '没有每日数据。')); return; }
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    const width = Math.max(650, Math.min(daily.length * 13, 1500));
    svg.setAttribute('viewBox', '0 0 ' + width + ' 210');
    svg.setAttribute('aria-hidden', 'true');
    const maximum = Math.max(1, ...daily.map((d) => d.requests || 0));
    const plotWidth = width - 65;
    const step = plotWidth / daily.length;
    function shape(tag, attributes, text) {
      const item = document.createElementNS(svg.namespaceURI, tag);
      for (const [key, value] of Object.entries(attributes)) item.setAttribute(key, String(value));
      if (text !== undefined) item.textContent = text;
      svg.append(item);
      return item;
    }
    for (let i = 0; i <= 4; i++) {
      const y = 15 + i * 40;
      shape('line', { x1: 46, x2: width - 10, y1: y, y2: y, class: 'axis' });
      shape('text', { x: 38, y: y + 4, 'text-anchor': 'end' }, n(maximum * (4 - i) / 4));
    }
    const labelStep = Math.max(1, Math.ceil(daily.length / 7));
    daily.forEach((d, i) => {
      const count = d.requests || 0;
      const height = count / maximum * 160;
      const x = 47 + i * step;
      const bar = shape('rect', { x, y: 175 - height, width: Math.max(1, step * .72), height, rx: Math.min(3, step / 4), class: 'bar' });
      const title = document.createElementNS(svg.namespaceURI, 'title');
      title.textContent = dateValue(new Date(d.start_at * 1000)) + '：' + n(count) + ' 次请求';
      bar.append(title);
      if (i % labelStep === 0 || i === daily.length - 1) shape('text', { x: x + step / 2, y: 199, 'text-anchor': 'middle' }, dateValue(new Date(d.start_at * 1000)).slice(5));
    });
    host.setAttribute('aria-label', daily.length + ' 天每日请求趋势，最大 ' + n(maximum) + ' 次。完整数值见每日数据表。');
    host.append(svg);
  }
  function statusBadge(status) { return node('span', 'badge ' + (['connecting', 'streaming'].includes(status) ? 'active' : status in statusNames ? status : ''), statusNames[status] || status || '未知'); }
  function costLabel(cost) {
    if (!isNumber(cost?.total_usd)) return '未知';
    return (cost.billing_basis === 'upstream_charge' ? '' : '≈ ') + usd(cost.total_usd);
  }
  function tokenLabel(record) {
    const details = record.token_details || {};
    const input = details.input_available === false ? '未知' : n(record.input_tokens);
    const output = details.output_available === false ? '未知' : n(record.output_tokens);
    return (details.input_estimated ? '≈' : '') + input + ' / ' + (details.output_estimated ? '≈' : '') + output;
  }
  function renderRequests() {
    $('requests-count').textContent = '已加载 ' + state.records.length + ' / ' + n(state.total) + ' 条 · 进行中 ' + n(state.active) + ' 条 · 每页最多 100 条';
    $('requests-table').replaceChildren(table(
      ['时间', '来源', '模型', '入口', '状态', 'HTTP', '输入 / 输出 token', 'USD', '完整耗时', 'TTFT', '输出 TPS'],
      state.records.map((r) => [button(stamp(r.started_at), () => openRequest(r.attempt_id), 'row-link'), r.source_name || r.source_id || '未记录', r.model || '未记录', r.ingress || '未知', statusBadge(r.status), r.http_status ?? '未记录', tokenLabel(r), costLabel(r.cost), ms(r.latency_ms), ms(r.first_token_latency_ms), n(r.output_tps)]),
      '当前筛选范围内没有请求记录。',
      state.records.map((r) => () => openRequest(r.attempt_id)),
    ));
    $('requests-more').hidden = state.next === null;
  }
  function renderCache() {
    $('cache-count').textContent = '已加载 ' + state.cacheRecords.length + ' / ' + n(state.total) + ' 条 · 网关保留旧快照 ' + n(state.legacy) + ' 条';
    $('cache-unfiltered').setAttribute('aria-pressed', String(state.unfiltered));
    $('cache-unfiltered').textContent = state.unfiltered ? '返回当前筛选范围' : '全部保留缓存（含旧快照）';
    $('cache-scope').textContent = state.unfiltered
      ? '正在显示全部保留缓存：忽略日期、来源、模型、入口和关联请求筛选。旧快照未记录的时间、来源与请求关联标记为“未记录”。'
      : state.attempt ? '关联尝试：' + state.attempt + '。正在显示此尝试的全部保留缓存，不额外限制日期、来源、模型或入口。'
        : '当前日期与来源筛选不包含缺少元数据的旧快照；请点“全部保留缓存”浏览它们，旧快照未被丢弃。';
    $('cache-table').replaceChildren(table(
      ['捕获时间', '来源', '模型', '入口', '请求 / 尝试', '类型', '读取'],
      state.cacheRecords.map((r) => [stamp(r.captured_at), r.source_name || r.source_id || '未记录', r.model || '未记录', r.ingress || '未记录', r.attempt_id || r.request_id || '未记录', node('span', 'badge', r.legacy ? '旧快照 · 关联未知' : '捕获记录'), button('打开原文', () => openCache(r.id), 'row-link')]),
      state.unfiltered ? '网关没有保留的原文缓存。' : '当前筛选下没有缓存。旧快照可能缺少筛选元数据，请使用上方“全部保留缓存”。',
    ));
    $('cache-more').hidden = state.next === null;
  }
  function mergeRecords(existing, incoming, key) {
    const merged = new Map(existing.map((item) => [String(item[key]), item]));
    for (const item of incoming) merged.set(String(item[key]), item);
    return [...merged.values()].sort((a, b) => (b.started_at ?? b.sequence ?? 0) - (a.started_at ?? a.sequence ?? 0));
  }
  async function load({ more = false, refresh = false, options = false } = {}) {
    const scope = controller('list');
    const epoch = state.epoch;
    const tab = state.tab;
    state.busy = true;
    $('refresh').disabled = true;
    $(tab === 'cache' ? 'cache-more' : 'requests-more').disabled = true;
    message(more ? '正在加载下一页…' : '正在读取本地数据…');
    try {
      if (tab === 'overview') {
        const [data, status] = await Promise.all([api('analytics', query('analytics'), scope.signal), api('status', {}, scope.signal)]);
        if (epoch !== state.epoch || scope.signal.aborted) return;
        renderOverview(data, status);
        message('已更新 · ' + stamp(data.fetched_at));
      } else {
        const params = { ...query(tab), limit: 100 };
        if (more && state.next !== null) params.before = state.next;
        const [data, optionData] = await Promise.all([
          api(tab, params, scope.signal),
          options ? api('analytics', query('analytics'), scope.signal) : Promise.resolve(null),
        ]);
        if (epoch !== state.epoch || scope.signal.aborted) return;
        if (optionData) renderOptions(optionData.options);
        const key = tab === 'requests' ? 'records' : 'cacheRecords';
        const incoming = data.records || [];
        const oldPages = state.pages;
        state[key] = more || refresh ? mergeRecords(state[key], incoming, tab === 'requests' ? 'attempt_id' : 'id') : incoming;
        if (refresh && tab === 'requests') {
          const incomingIds = new Set(incoming.map((r) => r.attempt_id));
          state.records = state.records.filter((r) => r.active !== true || incomingIds.has(r.attempt_id));
        }
        if (!refresh || oldPages <= 1) state.next = data.next_cursor ?? null;
        state.pages = more ? oldPages + 1 : refresh ? Math.max(1, oldPages) : 1;
        state.total = data.total;
        state.active = data.active_count;
        state.legacy = data.legacy_total;
        if (tab === 'requests') renderRequests(); else renderCache();
        message('已更新 · ' + stamp(data.fetched_at) + (refresh && oldPages > 1 ? ' · 已保留已加载的历史分页' : ''));
      }
    } catch (error) { report(error); }
    finally {
      if (epoch === state.epoch && state.controllers.get('list') === scope) {
        state.busy = false;
        $('refresh').disabled = false;
        $('requests-more').disabled = false;
        $('cache-more').disabled = false;
        state.controllers.delete('list');
      }
      updateRefreshState();
    }
  }
  function rawDetails(title, value, open = false) {
    const result = node('details');
    result.open = open;
    result.append(node('summary', '', title));
    let rendered = false;
    const render = () => {
      if (result.open && !rendered) {
        result.append(node('pre', 'json-text', json(value)));
        rendered = true;
      }
    };
    result.addEventListener('toggle', render);
    render();
    return result;
  }
  function section(title) {
    const result = node('section', 'detail-section');
    result.append(node('h3', '', title));
    return result;
  }
  function metrics(entries) {
    const result = node('dl', 'metrics-list');
    for (const [label, value] of entries) result.append(node('dt', '', label), node('dd', '', value));
    return result;
  }
  function closeDetail() {
    abort('detail');
    abort('cache-read');
    state.detail = null;
    $('detail-dialog').close();
    $('detail-content').replaceChildren();
    $('detail-title').textContent = '详情';
    $('detail-kind').textContent = '';
    updateRefreshState();
  }
  function beginDetail(kind, id) {
    abort('detail');
    abort('cache-read');
    state.lastFocus = document.activeElement;
    const detail = { kind, id, text: '', next: null, busy: false, all: false, status: null, reason: null, bytes: 0, total: null };
    state.detail = detail;
    $('detail-kind').textContent = kind === 'request' ? '请求尝试 / 诊断' : '保留原文 / 只读';
    $('detail-title').textContent = kind === 'request' ? '请求详情' : '原文缓存';
    $('detail-content').replaceChildren(node('p', 'empty', '正在读取…'));
    if (!$('detail-dialog').open) $('detail-dialog').showModal();
    updateRefreshState();
    return detail;
  }
  async function openRequest(id) {
    if (!id) { message('该记录未提供尝试标识，无法打开详情。', true); return; }
    const detail = beginDetail('request', id);
    const scope = controller('detail');
    try {
      const data = await api('requests/' + encodeURIComponent(id), { body_offset: 0, body_limit: 65536 }, scope.signal);
      if (state.detail !== detail) return;
      detail.data = data;
      detail.text = data.diagnostic?.body_text || '';
      detail.next = data.diagnostic?.body_next_offset ?? null;
      renderRequestDetail(detail);
    } catch (error) { report(error, $('detail-content')); }
  }
  function renderRequestDetail(detail) {
    const { record: r, diagnostic: d, related } = detail.data;
    const content = $('detail-content');
    content.replaceChildren();
    $('detail-title').textContent = r.model || '请求详情';
    const top = section('请求指标');
    top.append(metrics([
      ['尝试 ID', r.attempt_id || detail.id], ['请求 ID', r.request_id || '未记录'], ['开始时间', stamp(r.started_at)],
      ['来源', r.source_name || r.source_id || '未记录'], ['模型', r.model || '未记录'], ['入口', r.ingress || d?.ingress || '未知'],
      ['状态', statusNames[r.status] || r.status || '未知'], ['HTTP', String(r.http_status ?? d?.http_status ?? '未记录')],
      ['输入 / 输出', tokenLabel(r)], ['缓存输入', r.token_details?.cached_input_reported ? n(r.cached_input_tokens) : '未报告'],
      ['推理 token', n(r.token_details?.reasoning_output_tokens)], ['完整耗时', ms(r.latency_ms)], ['首 token 延迟', ms(r.first_token_latency_ms)],
      ['输出 TPS', n(r.output_tps)], ['输出阶段耗时', ms(r.generation_ms)], ['输出阶段 TPS', n(r.generation_tps)], ['费用 USD', costLabel(r.cost)],
    ]));
    top.append(node('p', 'compact-note', '输出 TPS 使用完整请求耗时；输出阶段 TPS 从首个输出计时。'));
    top.append(button('查看该尝试的原文缓存', () => linkCache(detail.id)));
    content.append(top);
    const token = section('Token 可用性与估算标记');
    token.append(rawDetails('全部 token_details 字段', r.token_details ?? null, true));
    content.append(token);
    const cost = section('费用来源与定价依据');
    cost.append(node('p', 'hint', '≈ 表示非上游账单金额；实际、估算、历史重算、未定价及部分费用以以下 provenance 原始字段为准。'));
    cost.append(rawDetails('完整费用字段', r.cost ?? null, true));
    content.append(cost);
    const diagnostic = section('诊断元数据与响应头');
    if (!d) diagnostic.append(node('p', 'warning', '未保留诊断信息；不能据此还原响应头或错误正文。'));
    else {
      const metadata = { ...d };
      delete metadata.body_text;
      delete metadata.body_next_offset;
      diagnostic.append(rawDetails('诊断元数据（含已保留响应头与正文状态）', metadata, true));
    }
    content.append(diagnostic);
    const body = section('错误响应正文（非请求正文）');
    const progress = node('p', 'chunk-progress');
    const bodyText = node('pre', 'body-text', detail.text || '没有可用的错误响应正文。');
    const actions = node('div', 'detail-actions');
    const more = button('继续读取错误正文', async () => {
      const scope = controller('detail');
      more.disabled = true;
      try {
        const data = await api('requests/' + encodeURIComponent(detail.id), { body_offset: detail.next, body_limit: 262144 }, scope.signal);
        if (state.detail !== detail) return;
        const next = data.diagnostic?.body_next_offset ?? null;
        if (next !== null && next <= detail.next) throw new Error('正文分页未前进，已停止读取。');
        detail.text += data.diagnostic?.body_text || '';
        detail.next = next;
        bodyText.textContent = detail.text || '没有可用的错误响应正文。';
        updateBody();
      } catch (error) { report(error, progress); }
      finally { more.disabled = false; }
    });
    const updateBody = () => {
      progress.textContent = '已读 ' + n(new TextEncoder().encode(detail.text).length) + ' 字节 · ' + (detail.next !== null ? '仍有更多分页。' : '已读完可用诊断正文；是否截断或淘汰以诊断元数据为准。');
      more.hidden = detail.next === null;
    };
    updateBody();
    actions.append(more);
    body.append(progress, bodyText, actions);
    content.append(body);
    const attempts = section('同一请求的关联尝试');
    const links = node('div', 'attempt-links');
    for (const attempt of related) {
      const id = attempt.attempt_id;
      const item = button((attempt.source_name || attempt.source_id || '未知来源') + ' · ' + (statusNames[attempt.status] || attempt.status || '未知') + ' · ' + (id || '标识缺失'), () => openRequest(id));
      item.disabled = !id || id === detail.id;
      links.append(item);
    }
    if (!related.length) links.append(node('p', 'hint', '没有可用的关联尝试。'));
    attempts.append(links);
    content.append(attempts, rawDetails('完整请求记录 · 原始 JSON', r));
  }
  function linkCache(id) {
    closeDetail();
    cancelPending();
    state.attempt = id;
    state.unfiltered = false;
    resetPages();
    activateTab('cache');
    load();
  }
  async function openCache(id) {
    const detail = beginDetail('cache', id);
    renderCacheDetail(detail);
    await readCachePage(detail);
  }
  function renderCacheDetail(detail) {
    const content = $('detail-content');
    content.replaceChildren();
    $('detail-title').textContent = '原文缓存';
    const metadata = section('捕获信息与完整性');
    const status = node('p', 'warning', '正在检查缓存状态…');
    const info = node('div');
    metadata.append(status, info);
    const reader = section('原文内容');
    const progress = node('p', 'chunk-progress');
    const actions = node('div', 'detail-actions');
    const more = button('读取下一块', () => readCachePage(detail));
    const all = button('读取全部', () => readAllCache(detail));
    const cancel = button('取消读取', () => {
      detail.all = false;
      abort('cache-read');
      detail.busy = false;
      updateCacheReader(detail);
    });
    const toggle = button('查看原始 JSON', () => {
      detail.showRaw = !detail.showRaw;
      updateCacheReader(detail);
    });
    const copy = button('复制已保留原文', async () => {
      try {
        await navigator.clipboard.writeText(detail.text);
        if (state.detail === detail) detail.ui.notice.textContent = '已复制全部已读取、可用的原文' + (detail.status === 'complete' ? '。' : '（此缓存不完整）。');
      } catch { if (state.detail === detail) detail.ui.notice.textContent = '剪贴板不可用。请展开原始 JSON 手动复制，或下载。'; }
    });
    const download = button('下载已保留原文', () => {
      const blob = new Blob([detail.text], { type: 'application/json;charset=utf-8' });
      const url = URL.createObjectURL(blob);
      const link = node('a');
      link.href = url;
      link.download = 'codexpress-cache-' + String(detail.id).replace(/[^a-zA-Z0-9_-]/g, '_') + (detail.status === 'complete' ? '' : '-incomplete') + '.json';
      document.body.append(link);
      link.click();
      link.remove();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    });
    const notice = node('p', 'hint');
    const raw = node('pre', 'json-text');
    raw.hidden = true;
    const readable = node('div');
    actions.append(more, all, cancel, toggle, copy, download);
    reader.append(progress, actions, notice, raw, readable);
    content.append(metadata, reader);
    detail.ui = { status, info, progress, more, all, cancel, toggle, copy, download, notice, raw, readable };
    updateCacheReader(detail);
  }
  async function readCachePage(detail) {
    if (state.detail !== detail || detail.busy || detail.finished) return;
    const scope = controller('cache-read');
    detail.busy = true;
    updateCacheReader(detail);
    try {
      const offset = detail.next ?? 0;
      const data = await api('cache/' + encodeURIComponent(detail.id), { offset, limit: 262144 }, scope.signal);
      if (state.detail !== detail) return;
      if (data.next_offset !== null && data.next_offset <= offset) throw new Error('缓存分页未前进，已停止读取。');
      detail.text += data.text;
      detail.next = data.next_offset ?? null;
      detail.finished = detail.next === null;
      detail.metadata = data.metadata;
      detail.status = data.status;
      detail.reason = data.reason;
      detail.total = data.total_bytes;
      detail.bytes += new TextEncoder().encode(data.text).length;
      detail.error = null;
    } catch (error) {
      if (error.name !== 'AbortError') { detail.error = error.message || '读取失败，请重试。'; detail.all = false; }
    } finally {
      if (state.detail === detail && state.controllers.get('cache-read') === scope) {
        detail.busy = false;
        state.controllers.delete('cache-read');
        updateCacheReader(detail);
      }
    }
  }
  async function readAllCache(detail) {
    if (detail.busy || detail.finished || state.detail !== detail) return;
    detail.all = true;
    while (state.detail === detail && detail.all && !detail.finished) {
      await readCachePage(detail);
      if (detail.error || !detail.all || state.detail !== detail) break;
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    detail.all = false;
    if (state.detail === detail) updateCacheReader(detail);
  }
  function updateCacheReader(detail) {
    if (state.detail !== detail || !detail.ui) return;
    const ui = detail.ui;
    ui.status.textContent = detail.status === 'complete'
      ? (detail.finished ? '完整缓存 · 已读完所有分页。' : '网关标记缓存完整；页面尚未读完，请继续读取。')
      : detail.status === 'incomplete' ? '缓存不完整 · ' + detail.reason : '正在读取缓存状态…';
    ui.status.className = detail.status === 'complete' && detail.finished ? 'success-text' : 'warning';
    if (detail.metadata !== undefined && ui.info.childElementCount === 0) ui.info.append(rawDetails('全部捕获元数据', detail.metadata, true));
    ui.progress.textContent = '已加载 ' + n(detail.bytes) + ' / ' + n(detail.total) + ' 字节 · ' + (detail.finished ? '已读完可用分页' : detail.busy ? '正在读取…' : '尚未读完');
    ui.more.hidden = Boolean(detail.finished);
    ui.all.hidden = Boolean(detail.finished);
    ui.more.disabled = detail.busy || detail.all;
    ui.all.disabled = detail.busy || detail.all;
    ui.cancel.hidden = !detail.busy && !detail.all;
    ui.copy.disabled = !detail.finished || !detail.text;
    ui.download.disabled = !detail.finished || !detail.text;
    ui.toggle.textContent = detail.showRaw ? '查看可读内容' : '查看原始 JSON';
    ui.raw.hidden = !detail.showRaw;
    ui.readable.hidden = Boolean(detail.showRaw);
    if (detail.showRaw) ui.raw.textContent = detail.text || '尚无可用原文。';
    if (detail.error) ui.notice.textContent = detail.error;
    else if (!detail.finished) ui.notice.textContent = '内容尚未读完；原始视图仅为已加载片段。读完全部分页后才启用复制、下载及结构化阅读。';
    else if (detail.status !== 'complete') ui.notice.textContent = '以下仅是已验证的可用数据，不代表完整请求。原因见上方；下载文件名将标记 incomplete。';
    else ui.notice.textContent = '全部原文已读取。图片、文件及链接不会自动请求；加密字段保持原样，面板无法解密。';
    if (detail.finished && detail.renderedText !== detail.text) {
      detail.renderedText = detail.text;
      ui.readable.replaceChildren();
      if (!detail.text) ui.readable.append(node('p', 'empty', '没有可用原文；请查看完整性状态及原因。'));
      else {
        try { renderCanonical(ui.readable, JSON.parse(detail.text)); }
        catch {
          ui.readable.append(node('p', 'warning', '可用文本无法解析为完整 JSON。以下按保留原样显示，不补造缺失结构。'), node('pre', 'body-text', detail.text));
        }
      }
    } else if (!detail.finished && !ui.readable.childElementCount) ui.readable.append(node('p', 'empty', '读完可用分页后显示可读内容；现在可切换原始 JSON 查看已加载片段。'));
  }
  const fieldNames = {
    instructions: '系统指令', tools: '工具定义', items: '消息与事件', content: '内容', text: '文本', input_text: '输入文本', output_text: '输出文本',
    arguments: '工具调用参数', output: '工具结果', result: '结果', reasoning: '推理', summary: '摘要', encrypted_content: '加密内容（不透明，面板无法解密）',
    image_url: '图像地址（仅展示，不请求）', file_data: '文件数据（仅展示）', compaction: '压缩上下文', role: '角色', type: '类型',
  };
  function renderValue(host, key, value, depth = 0) {
    const label = fieldNames[key] ? fieldNames[key] + ' · ' + key : key;
    if (value === null || typeof value !== 'object') {
      const text = typeof value === 'string' ? value : json(value);
      const large = text.length > 8000;
      const group = node(large ? 'details' : 'div', 'content-node');
      group.append(node(large ? 'summary' : 'p', large ? '' : 'hint', label + (large ? ' · ' + n(text.length) + ' 字符，展开完整内容' : '')));
      if (key === 'encrypted_content') group.append(node('p', 'opaque', '加密原文，无法解密；保留值完整展示。'));
      let rendered = false;
      const render = () => {
        if (rendered || (large && !group.open)) return;
        group.append(node('pre', 'readable-text', text));
        rendered = true;
      };
      if (large) group.addEventListener('toggle', render);
      render();
      host.append(group);
      return;
    }
    const entries = Array.isArray(value) ? value.map((v, i) => [String(i + 1), v]) : Object.entries(value);
    const group = node('details', 'content-node');
    group.append(node('summary', '', label + ' · ' + entries.length + (Array.isArray(value) ? ' 项' : ' 字段')));
    let rendered = false;
    group.open = depth === 0 && entries.length <= 8;
    const render = () => {
      if (!group.open || rendered) return;
      rendered = true;
      if (!entries.length) group.append(node('pre', 'readable-text', json(value)));
      else for (const [childKey, childValue] of entries) renderValue(group, childKey, childValue, depth + 1);
    };
    group.addEventListener('toggle', render);
    render();
    host.append(group);
  }
  function renderCanonical(host, data) {
    if (!data || typeof data !== 'object' || Array.isArray(data)) { renderValue(host, '原文', data); return; }
    for (const [key, value] of Object.entries(data)) {
      if (key !== 'items' || !Array.isArray(value)) { renderValue(host, key, value); continue; }
      host.append(node('h3', '', '消息与事件 · ' + value.length + ' 项'));
      if (!value.length) host.append(node('p', 'hint', '无消息条目。'));
      const list = node('div');
      let shown = 0;
      const more = button('展示更多消息', () => appendBatch());
      const appendItem = (item, index) => {
        const container = node('details', 'reader-item');
        const role = item?.role || item?.message?.role;
        const type = item?.type;
        container.append(node('summary', '', String(index + 1).padStart(2, '0') + ' · ' + (role || '角色未记录') + ' · ' + (type || '类型未记录')));
        let rendered = false;
        container.open = value.length <= 12 && index < 3;
        const render = () => {
          if (!container.open || rendered) return;
          rendered = true;
          if (item && typeof item === 'object') for (const [field, content] of Object.entries(item)) renderValue(container, field, content);
          else renderValue(container, '原始条目', item);
        };
        container.addEventListener('toggle', render);
        render();
        list.append(container);
      };
      const appendBatch = () => {
        const end = Math.min(shown + 50, value.length);
        while (shown < end) {
          appendItem(value[shown], shown);
          shown++;
        }
        more.hidden = shown >= value.length;
        more.textContent = '展示更多消息（已展示 ' + shown + ' / ' + value.length + '）';
      };
      host.append(list, more);
      appendBatch();
    }
  }
  function resetPages() {
    state.records = [];
    state.cacheRecords = [];
    state.next = null;
    state.pages = 0;
    state.total = 0;
    state.active = 0;
    state.legacy = 0;
    $('requests-table').replaceChildren();
    $('cache-table').replaceChildren();
    $('requests-count').textContent = '';
    $('cache-count').textContent = '';
    $('requests-more').hidden = true;
    $('cache-more').hidden = true;
  }
  function activateTab(tab) {
    state.tab = tab;
    for (const name of tabs) {
      $('panel-' + name).hidden = name !== tab;
      $('tab-' + name).setAttribute('aria-selected', String(name === tab));
      $('tab-' + name).tabIndex = name === tab ? 0 : -1;
    }
    $('page-title').textContent = titles[tab];
    $('page-subtitle').textContent = subtitles[tab];
    $('cache-filter-note').hidden = tab !== 'cache';
    updateRefreshState();
  }
  function selectTab(tab) {
    if (state.tab === tab) return;
    closeDetail();
    cancelPending();
    resetPages();
    state.unfiltered = false;
    state.attempt = '';
    activateTab(tab);
    load();
  }
  function applyFilters() {
    let filter;
    try { filter = readFilters(); } catch (error) { message(error.message, true); return; }
    closeDetail();
    cancelPending();
    state.filter = filter;
    state.unfiltered = false;
    state.attempt = '';
    resetPages();
    for (const id of ['kpis', 'groups', 'daily-chart', 'daily-table']) $(id).replaceChildren();
    load({ options: true });
  }
  function updateRefreshState() {
    let label = '';
    if ($('auto-refresh').checked) {
      label = document.hidden ? '自动刷新已暂停：页面不可见' : state.detail ? '自动刷新已暂停：正在阅读详情' : state.pages > 1 ? '自动刷新已暂停：保留历史分页' : state.busy ? '正在读取' : '自动刷新已开启 · 15 秒';
    } else label = '自动刷新已关闭';
    $('refresh-state').textContent = label;
  }
  $('filters').addEventListener('submit', (event) => { event.preventDefault(); applyFilters(); });
  $('reset').addEventListener('click', () => {
    for (const id of ['source', 'model', 'ingress', 'status', 'search']) $('filter-' + id).value = '';
    quickRange(30);
    applyFilters();
  });
  document.querySelectorAll('[data-days]').forEach((item) => item.addEventListener('click', () => { quickRange(Number(item.dataset.days)); applyFilters(); }));
  for (const id of ['filter-from', 'filter-to']) $(id).addEventListener('change', () => document.querySelectorAll('[data-days]').forEach((item) => item.setAttribute('aria-pressed', 'false')));
  for (const tab of tabs) $('tab-' + tab).addEventListener('click', () => selectTab(tab));
  document.querySelector('[role="tablist"]').addEventListener('keydown', (event) => {
    const index = tabs.indexOf(state.tab);
    let next;
    if (event.key === 'ArrowDown' || event.key === 'ArrowRight') next = tabs[(index + 1) % tabs.length];
    if (event.key === 'ArrowUp' || event.key === 'ArrowLeft') next = tabs[(index + tabs.length - 1) % tabs.length];
    if (event.key === 'Home') next = tabs[0];
    if (event.key === 'End') next = tabs[tabs.length - 1];
    if (next) { event.preventDefault(); selectTab(next); $('tab-' + next).focus(); }
  });
  $('requests-more').addEventListener('click', () => load({ more: true }));
  $('cache-more').addEventListener('click', () => load({ more: true }));
  $('cache-unfiltered').addEventListener('click', () => {
    cancelPending();
    state.unfiltered = !state.unfiltered;
    state.attempt = '';
    resetPages();
    load();
  });
  $('refresh').addEventListener('click', () => load({ refresh: state.tab !== 'overview' }));
  $('auto-refresh').addEventListener('change', updateRefreshState);
  document.addEventListener('visibilitychange', updateRefreshState);
  setInterval(() => {
    if ($('auto-refresh').checked && !document.hidden && !state.busy && !state.detail && state.pages <= 1) load({ refresh: state.tab !== 'overview' });
  }, 15000);
  $('detail-close').addEventListener('click', closeDetail);
  $('detail-dialog').addEventListener('cancel', (event) => { event.preventDefault(); closeDetail(); });
  $('detail-dialog').addEventListener('close', () => {
    if (state.detail) closeDetail();
    if (state.lastFocus?.isConnected) state.lastFocus.focus();
  });
  $('theme').addEventListener('click', () => {
    const dark = document.body.dataset.theme ? document.body.dataset.theme === 'dark' : window.matchMedia('(prefers-color-scheme: dark)').matches;
    document.body.dataset.theme = dark ? 'light' : 'dark';
  });
  window.addEventListener('pagehide', () => { cancelPending(); closeDetail(); });
  $('timezone').textContent = '时区：' + (Intl.DateTimeFormat().resolvedOptions().timeZone || '本地时区') + ' · 按本地午夜分日';
  quickRange(30);
  state.filter = readFilters();
  activateTab('overview');
  load();
})();
