// epubkit — desktop front-end.
//
// The page holds no processing logic and no idea what a preset means. It
// renders whatever `settings` the core hands back, and asks the core to change
// it. That way the window and the CLI cannot drift apart.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open } = window.__TAURI__.dialog;
const { revealItemInDir } = window.__TAURI__.opener;

const BUILTIN_PRESETS = [
    { id: 'quick', label: 'Quick', icon: '⚡', description: 'Images + text' },
    { id: 'full', label: 'Full', icon: '✨', description: 'Device-optimized' },
];

const dropZone = document.getElementById('drop-zone');
const fileList = document.getElementById('file-list');
const optionsPanel = document.getElementById('options-panel');
const progressSection = document.getElementById('progress-section');
const progressItems = document.getElementById('progress-items');
const resultsSection = document.getElementById('results-section');
const resultsItems = document.getElementById('results-items');
const optimizeBtn = document.getElementById('optimize-btn');
const stopBtn = document.getElementById('stop-btn');
const qualitySlider = document.getElementById('quality');
const qualityValue = document.getElementById('quality-value');
const statusLine = document.getElementById('status-line');
const savePresetBtn = document.getElementById('save-preset-btn');
const deletePresetBtn = document.getElementById('delete-preset-btn');
const filenameTemplate = document.getElementById('filename-template');
const filenameExample = document.getElementById('filename-example');

/** Books currently listed, in the order they were added. */
let books = [];
/** The core's settings. Never edited except through `applySettings`. */
let settings = null;
let devices = [];
let running = false;
/** Set once Stop is pressed, until the run has stopped. */
let stopping = false;
/** Whether the footer shows the ready line, not a message to be read. */
let readyShown = false;

// ------------------------------------------------------------------- startup

async function start() {
    try {
        [settings, devices] = await Promise.all([
            invoke('load_settings'),
            invoke('devices'),
        ]);
    } catch (error) {
        // Without settings there is nothing sensible to show, so say so plainly
        // rather than rendering a window that silently does the wrong thing.
        statusLine.textContent = `Could not load settings: ${error}`;
        statusLine.classList.add('error-text');
        return;
    }

    renderDevices();
    renderPresets();
    renderOptions();
    renderFilename();
    wireEvents();
    showReady();
}

/// The footer carries the current device and preset, so what a run will do is
/// visible without opening the options panel.
function showReady() {
    const device = devices.find((d) => d.id === settings.device);
    const name = device ? `${device.label} · ${device.width}×${device.height}` : settings.device;
    setStatus(`Ready · ${name} · ${presetLabel()}`);
    readyShown = true;
}

function presetLabel() {
    if (settings.active === 'quick') return 'Quick';
    if (settings.active === 'full') return 'Full';
    if (settings.active === 'custom') return 'Custom';
    return settings.presets.find((p) => p.id === settings.active)?.name ?? 'Custom';
}

function applySettings(next) {
    settings = next;
    renderPresets();
    renderOptions();
    renderFilename();
    showReady();
}

// ------------------------------------------------------------------ settings

function renderDevices() {
    const toggle = document.getElementById('device-toggle');
    toggle.innerHTML = '';

    for (const device of devices) {
        const button = document.createElement('button');
        button.className = `device-btn ${device.id === settings.device ? 'active' : ''}`;
        button.innerHTML = `
            <span class="device-name">${escapeHtml(device.id.toUpperCase())}</span>
            <span class="device-desc">${device.width}&times;${device.height}</span>`;
        button.addEventListener('click', () => {
            settings.device = device.id;
            persist();
            renderDevices();
            showReady();
        });
        toggle.appendChild(button);
    }
}

function renderPresets() {
    const bar = document.getElementById('preset-bar');
    bar.innerHTML = '';

    const saved = settings.presets.map((preset) => ({
        id: preset.id,
        label: preset.name,
        icon: '★',
        description: 'Saved',
    }));
    const custom = { id: 'custom', label: 'Custom', icon: '🔧', description: 'Pick & choose' };

    for (const preset of [...BUILTIN_PRESETS, ...saved, custom]) {
        const button = document.createElement('button');
        button.className = `preset-btn ${preset.id === settings.active ? 'active' : ''}`;
        button.innerHTML = `
            <span class="preset-icon">${preset.icon}</span>
            <span class="preset-label">${escapeHtml(preset.label)}</span>
            <span class="preset-desc">${escapeHtml(preset.description)}</span>`;
        button.addEventListener('click', () => selectPreset(preset.id));
        bar.appendChild(button);
    }

    // Only a preset the user saved can be deleted.
    deletePresetBtn.hidden = !settings.presets.some((p) => p.id === settings.active);
}

async function selectPreset(id) {
    if (id === 'custom') {
        // Custom means "keep what is on screen"; there is nothing to restore.
        settings.active = 'custom';
        persist();
        renderPresets();
        return;
    }

    try {
        // The core starts from the settings file, so it has to be up to date.
        await flushPersist();
        applySettings(await invoke('select_preset', { id }));
    } catch (error) {
        setStatus(`Could not select that preset: ${error}`, true);
    }
}

function renderOptions() {
    for (const input of document.querySelectorAll('[data-option]')) {
        input.checked = Boolean(settings.options[input.dataset.option]);
    }
    setQuality(settings.options.quality, false);
}

function setQuality(value, markCustom = true) {
    qualitySlider.value = value;
    qualityValue.textContent = `${value}%`;

    for (const button of document.querySelectorAll('.quality-btn')) {
        button.classList.toggle('active', Number(button.dataset.quality) === Number(value));
    }

    if (markCustom) {
        settings.options.quality = Number(value);
        markCustomized();
    }
}

/// Any hand edit moves the selection to Custom, so the UI stops claiming a
/// preset is in effect when it no longer is.
function markCustomized() {
    settings.active = 'custom';
    renderPresets();
    showReady();
    persist();
}

/// How finished books are named is remembered apart from the presets, like
/// the device, so choosing one leaves it alone.
function renderFilename() {
    const { format, template } = settings.filename;

    for (const button of document.querySelectorAll('[data-filename-format]')) {
        button.classList.toggle('active', button.dataset.filenameFormat === format);
    }
    document.getElementById('filename-template-row').hidden = format !== 'custom';

    // Rewriting the field while it is typed in would move the caret.
    if (document.activeElement !== filenameTemplate) filenameTemplate.value = template;
    if (format === 'custom') checkTemplate();
}

let templateChecks = 0;

/// Show what the template makes of an example book, or what is wrong with it.
async function checkTemplate() {
    const check = ++templateChecks;
    let message;
    let ok = true;

    try {
        message = `e.g. ${await invoke('check_filename_template', { template: settings.filename.template })}`;
    } catch (error) {
        message = `${error}`;
        ok = false;
    }

    // Answers can arrive out of order while typing; only the latest counts.
    if (check !== templateChecks) return;
    filenameExample.textContent = message;
    filenameExample.classList.toggle('error-text', !ok);
}

let persistTimer = null;

function persist() {
    // Dragging the quality slider fires on every pixel, so coalesce writes
    // rather than rewriting the settings file dozens of times a second.
    clearTimeout(persistTimer);
    persistTimer = setTimeout(() => {
        persistTimer = null;
        save();
    }, 250);
}

/// Write a change that is still waiting, and wait for any save under way, for
/// a command that reads the file.
async function flushPersist() {
    if (persistTimer !== null) {
        clearTimeout(persistTimer);
        persistTimer = null;
        save();
    }
    await saving;
}

/** The save last begun, for a read of the file to wait for. */
let saving = Promise.resolve();

function save() {
    saving = invoke('save_settings', { settings }).catch((error) => {
        setStatus(`Could not save settings: ${error}`, true);
    });
    return saving;
}

/// Read the settings again as the window comes back to the front: the
/// command line may have changed them meanwhile, and the page's copy, saved
/// whole at its next change, would undo what it did.
async function reloadSettings() {
    try {
        await flushPersist();
        const next = await invoke('load_settings');
        const wasReady = readyShown;
        settings = next;
        renderDevices();
        renderPresets();
        renderOptions();
        renderFilename();
        // A run's outcome, or an error, stays in the footer until it is read.
        if (wasReady) showReady();
    } catch (error) {
        setStatus(`Could not load settings: ${error}`, true);
    }
}

// --------------------------------------------------------------------- books

async function addBooks(paths) {
    const fresh = paths.filter(
        (path) => path.toLowerCase().endsWith('.epub') && !books.some((b) => b.path === path),
    );
    if (fresh.length === 0) return;

    setStatus(`Reading ${fresh.length} book${fresh.length === 1 ? '' : 's'}…`);

    try {
        const inspected = await invoke('inspect_books', { paths: fresh });
        books.push(...inspected);
        renderBooks();
        showReady();
    } catch (error) {
        setStatus(`Could not read those files: ${error}`, true);
    }
}

function renderBooks() {
    fileList.hidden = books.length === 0;
    optionsPanel.hidden = books.length === 0;
    fileList.innerHTML = '';

    books.forEach((book, index) => {
        const card = document.createElement('div');
        card.className = `file-card ${book.error ? 'error' : ''}`;

        const cover = book.cover
            ? `<img src="${escapeAttr(book.cover)}" alt="">`
            : '<div class="no-cover">No cover</div>';

        const meta = [book.author, book.series].filter(Boolean).join(' — ');
        const edit = book.error
            ? ''
            : `<div class="file-edit">
                   <input type="text" placeholder="Title" value="${escapeAttr(book.title)}" data-edit="title" data-index="${index}">
                   <input type="text" placeholder="Author" value="${escapeAttr(book.author)}" data-edit="author" data-index="${index}">
               </div>`;

        card.innerHTML = `
            <div class="file-cover">${cover}</div>
            <div class="file-info">
                <div class="file-name">${escapeHtml(book.title || book.filename)}</div>
                <div class="file-meta">${escapeHtml(meta)}${meta ? ' · ' : ''}${formatBytes(book.size)}</div>
                ${book.error ? `<div class="file-error">${escapeHtml(book.error)}</div>` : ''}
                ${edit}
            </div>
            <button class="file-remove" data-remove="${index}" title="Remove">&times;</button>`;

        fileList.appendChild(card);
    });
}

// ---------------------------------------------------------------- processing

async function optimize() {
    const jobs = books
        .map((book, index) => ({ book, index }))
        .filter(({ book }) => !book.error)
        .map(({ book }) => ({ path: book.path, title: book.title, author: book.author }));

    if (jobs.length === 0) {
        setStatus('Nothing to do — every book listed has a problem.', true);
        return;
    }

    // Every book would fail on a template the core refuses, so say so first.
    if (settings.filename.format === 'custom') {
        try {
            await invoke('check_filename_template', { template: settings.filename.template });
        } catch (error) {
            setStatus(`${error}`, true);
            filenameTemplate.focus();
            return;
        }
    }

    const destination = await open({
        directory: true,
        multiple: false,
        title: 'Where should the optimized books go?',
    });
    if (!destination) return;

    running = true;
    optimizeBtn.disabled = true;
    optimizeBtn.textContent = 'Processing…';
    stopBtn.hidden = false;
    stopBtn.disabled = false;
    stopBtn.textContent = 'Stop';
    resultsSection.hidden = true;
    resultsItems.innerHTML = '';
    progressSection.hidden = false;
    progressItems.innerHTML = '';

    for (const job of jobs) {
        const item = document.createElement('div');
        item.className = 'progress-item';
        item.dataset.path = job.path;
        item.innerHTML = `
            <div class="filename">${escapeHtml(basename(job.path))}</div>
            <div class="progress-bar-container"><div class="progress-bar"></div></div>
            <div class="progress-message">Waiting…</div>`;
        progressItems.appendChild(item);
    }

    stopping = false;
    setStatus(`Optimizing book 1 of ${jobs.length}…`);

    try {
        const outcomes = await invoke('optimize_books', { jobs, destination, settings });
        showResults(outcomes);
        setStatus(tally(outcomes));
    } catch (error) {
        setStatus(`Processing failed: ${error}`, true);
    } finally {
        running = false;
        optimizeBtn.disabled = false;
        optimizeBtn.textContent = 'Optimize';
        stopBtn.hidden = true;
    }
}

/// Every book's result, in the order the books were run, as the run ends;
/// each was shown as its book finished.
function showResults(outcomes) {
    resultsItems.replaceChildren(...outcomes.map(resultCard));
    resultsSection.hidden = false;
    resultsSection.scrollIntoView({ behavior: 'smooth', block: 'start' });
}

/// What a run came to, for the footer: "9 optimized, 1 failed".
function tally(outcomes) {
    const count = (kind) => outcomes.filter((outcome) => outcomeKind(outcome) === kind).length;
    const parts = [`${count('success')} optimized`];
    if (count('error') > 0) parts.push(`${count('error')} failed`);
    if (count('stopped') > 0) parts.push(`${count('stopped')} stopped`);
    return parts.join(', ');
}

function resultCard(outcome) {
    const card = document.createElement('div');
    card.className = `result-card ${outcomeKind(outcome)}`;

    if (outcome.error) {
        card.innerHTML = `
            <div class="result-header"><span class="filename">${escapeHtml(basename(outcome.path))}</span></div>
            <div class="file-error">${escapeHtml(outcome.error)}</div>`;
        return card;
    }

    const report = outcome.report;
    const change = report.originalSize > 0
        ? (1 - report.optimizedSize / report.originalSize) * 100
        : 0;
    // Dithering to four levels is noise to a DCT codec, so a book of
    // smooth artwork can legitimately come out bigger.
    const label = change < 0 ? 'larger' : 'smaller';

    // The name written, which is the report's unless that was taken.
    const written = outcome.output ? basename(outcome.output) : report.outputFilename;

    card.innerHTML = `
        <div class="result-header"><span class="filename">${escapeHtml(written)}</span></div>
        <div class="result-size">
            <span class="size-original">${formatBytes(report.originalSize)}</span>
            <span class="size-arrow">&rarr;</span>
            <span class="size-new">${formatBytes(report.optimizedSize)}</span>
            <span class="size-reduction">${Math.abs(change).toFixed(1)}% ${label}</span>
        </div>
        <div class="result-summary">${escapeHtml(outcome.summary)}</div>
        <div class="result-output">
            <span class="path" title="${escapeAttr(outcome.output ?? '')}">${escapeHtml(outcome.output ?? '')}</span>
            <button class="reveal-btn" data-reveal="${escapeAttr(outcome.output ?? '')}">Show</button>
        </div>`;
    return card;
}

// -------------------------------------------------------------------- wiring

function wireEvents() {
    dropZone.addEventListener('click', async () => {
        const chosen = await open({
            multiple: true,
            filters: [{ name: 'EPUB', extensions: ['epub'] }],
        });
        if (chosen) await addBooks(Array.isArray(chosen) ? chosen : [chosen]);
    });

    // Tauri delivers OS drag-and-drop as a window event, not a DOM one.
    listen('tauri://drag-drop', (event) => addBooks(event.payload.paths ?? []));
    listen('tauri://focus', reloadSettings);
    listen('tauri://drag-enter', () => dropZone.classList.add('dragover'));
    listen('tauri://drag-leave', () => dropZone.classList.remove('dragover'));
    listen('tauri://drag-drop', () => dropZone.classList.remove('dragover'));

    listen('progress', ({ payload }) => {
        if (running && !stopping) {
            setStatus(`Optimizing book ${payload.index + 1} of ${payload.total}…`);
        }
        const item = progressItems.querySelector(`[data-path="${cssEscape(payload.path)}"]`);
        if (!item) return;
        item.querySelector('.progress-bar').style.width = `${payload.percent}%`;
        item.querySelector('.progress-message').textContent = payload.message;
    });

    // Each book's result shows as soon as it is known, not when the last
    // book is done.
    listen('finished', ({ payload }) => {
        if (running) {
            resultsItems.appendChild(resultCard(payload));
            resultsSection.hidden = false;
        }
        const item = progressItems.querySelector(`[data-path="${cssEscape(payload.path)}"]`);
        if (!item) return;
        const kind = outcomeKind(payload);
        item.querySelector('.progress-bar').classList.add(kind === 'success' ? 'complete' : kind);
        if (payload.error) item.querySelector('.progress-message').textContent = payload.error;
    });

    stopBtn.addEventListener('click', async () => {
        stopping = true;
        stopBtn.disabled = true;
        stopBtn.textContent = 'Stopping…';
        setStatus('Stopping…');
        try {
            await invoke('stop_run');
        } catch (error) {
            setStatus(`Could not stop: ${error}`, true);
        }
    });

    fileList.addEventListener('click', (event) => {
        const remove = event.target.closest('[data-remove]');
        if (!remove || running) return;
        books.splice(Number(remove.dataset.remove), 1);
        renderBooks();
    });

    fileList.addEventListener('input', (event) => {
        const field = event.target.closest('[data-edit]');
        if (!field) return;
        books[Number(field.dataset.index)][field.dataset.edit] = field.value;
    });

    for (const input of document.querySelectorAll('[data-option]')) {
        input.addEventListener('change', () => {
            settings.options[input.dataset.option] = input.checked;
            markCustomized();
        });
    }

    qualitySlider.addEventListener('input', () => setQuality(qualitySlider.value));
    for (const button of document.querySelectorAll('.quality-btn')) {
        button.addEventListener('click', () => setQuality(Number(button.dataset.quality)));
    }

    for (const button of document.querySelectorAll('[data-filename-format]')) {
        button.addEventListener('click', () => {
            settings.filename.format = button.dataset.filenameFormat;
            renderFilename();
            persist();
            if (settings.filename.format === 'custom') filenameTemplate.focus();
        });
    }
    filenameTemplate.addEventListener('input', () => {
        settings.filename.template = filenameTemplate.value;
        checkTemplate();
        persist();
    });

    // `prompt()` is a no-op in the macOS webview, so the name is asked for
    // inline instead.
    const namer = document.getElementById('preset-namer');
    const nameField = document.getElementById('preset-name');

    const closeNamer = () => {
        namer.hidden = true;
        nameField.value = '';
    };

    const commitName = async () => {
        const name = nameField.value.trim();
        if (!name) return;
        try {
            applySettings(await invoke('save_preset', { name, settings }));
            setStatus(`Saved "${name}".`);
            closeNamer();
        } catch (error) {
            setStatus(`Could not save that preset: ${error}`, true);
        }
    };

    savePresetBtn.addEventListener('click', () => {
        namer.hidden = false;
        nameField.focus();
    });
    document.getElementById('preset-confirm').addEventListener('click', commitName);
    document.getElementById('preset-cancel').addEventListener('click', closeNamer);
    nameField.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') commitName();
        if (event.key === 'Escape') closeNamer();
    });

    deletePresetBtn.addEventListener('click', async () => {
        const id = settings.active;
        try {
            await flushPersist();
            applySettings(await invoke('delete_preset', { id }));
            setStatus('Preset deleted.');
        } catch (error) {
            setStatus(`Could not delete that preset: ${error}`, true);
        }
    });

    resultsItems.addEventListener('click', async (event) => {
        const button = event.target.closest('[data-reveal]');
        if (!button || !button.dataset.reveal) return;
        try {
            await revealItemInDir(button.dataset.reveal);
        } catch (error) {
            setStatus(`Could not open that folder: ${error}`, true);
        }
    });

    document.getElementById('clear-btn').addEventListener('click', () => {
        books = [];
        renderBooks();
        progressSection.hidden = true;
        resultsSection.hidden = true;
        progressItems.innerHTML = '';
        resultsItems.innerHTML = '';
        showReady();
    });

    optimizeBtn.addEventListener('click', optimize);
}

// ------------------------------------------------------------------ helpers

/// How a book's run ended: made, failed, or stopped before it was made.
function outcomeKind(outcome) {
    if (outcome.stopped) return 'stopped';
    return outcome.error ? 'error' : 'success';
}

function setStatus(message, isError = false) {
    statusLine.textContent = message;
    statusLine.classList.toggle('error-text', isError);
    readyShown = false;
}

function basename(path) {
    return path.split(/[\\/]/).pop();
}

function formatBytes(bytes) {
    if (!bytes) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    return `${(bytes / 1024 ** i).toFixed(1)} ${units[i]}`;
}

function escapeHtml(value) {
    const div = document.createElement('div');
    div.textContent = value ?? '';
    return div.innerHTML;
}

function escapeAttr(value) {
    return escapeHtml(value).replace(/"/g, '&quot;');
}

function cssEscape(value) {
    return window.CSS && CSS.escape ? CSS.escape(value) : value.replace(/["\\]/g, '\\$&');
}

start();
