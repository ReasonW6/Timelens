const invoke = window.__TAURI__.core.invoke;
const status = document.querySelector("#status");
const track = document.querySelector("#track");
const pool = [];

for (let index = 0; index < 512; index += 1) {
  const item = document.createElement("div");
  item.className = "segment";
  track.appendChild(item);
  pool.push(item);
}

function renderVisible(segments, panMinute, zoom) {
  const right = panMinute + 1100 / zoom;
  let visibleIndex = 0;
  for (const segment of segments) {
    const end = segment.startMinute + segment.durationMinutes;
    if (end < panMinute || segment.startMinute > right) continue;
    if (visibleIndex === pool.length) throw new Error("visible segment pool exhausted");
    const item = pool[visibleIndex];
    item.className = `segment ${segment.state}`;
    item.style.display = "block";
    item.style.left = `${(segment.startMinute - panMinute) * zoom}px`;
    item.style.top = `${segment.lane * 42 + 12}px`;
    item.style.width = `${Math.max(1, segment.durationMinutes * zoom)}px`;
    visibleIndex += 1;
  }
  for (; visibleIndex < pool.length; visibleIndex += 1) {
    pool[visibleIndex].style.display = "none";
  }
}

async function start() {
  const segments = JSON.parse(await invoke("load_timeline"));
  renderVisible(segments, 0, 1);

  let step = 0;
  const started = performance.now();
  let lastTick = started;
  let maxStepGapMs = 0;
  let lateStepCount = 0;
  const timer = setInterval(() => {
    const now = performance.now();
    const gap = now - lastTick;
    lastTick = now;
    maxStepGapMs = Math.max(maxStepGapMs, gap);
    if (gap > 25) lateStepCount += 1;
    step += 1;
    const panMinute = (step * 53) % 42000;
    const zoom = 0.8 + (step % 40) * 0.015;
    renderVisible(segments, panMinute, zoom);
    status.textContent = `Pan ${panMinute} min · Zoom ${Math.round(zoom * 100)}%`;
    if (step === 240) {
      clearInterval(timer);
      awaitMetric(performance.now() - started, maxStepGapMs, lateStepCount);
    }
  }, 16);
}

async function awaitMetric(elapsedMs, maxStepGapMs, lateStepCount) {
  await invoke("record_pan", { elapsedMs, maxStepGapMs, lateStepCount });
}

start();
