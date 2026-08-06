// epubkit — desktop front-end.
//
// The page holds no processing logic and no idea what a preset means. It
// renders whatever `settings` the core hands back, and asks the core to change
// it. That way the window and the CLI cannot drift apart.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open } = window.__TAURI__.dialog;

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
const qualitySlider = document.getElementById('quality');
const qualityValue = document.getElementById('quality-value');
const statusLine = document.getElementById('status-line');
const savePresetBtn = document.getElementById('save-preset-btn');
const deletePresetBtn = document.getElementById('delete-preset-btn');

/** Books currently listed, in the order they were added. */
let books = [];
/** The core's settings. Never edited except through `applySettings`. */
let settings = null;
let devices = [];
let running = false;

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
    wireEvents();
}

function applySettings(next) {
    settings = next;
    renderPresets();
    renderOptions();
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
        button.addEventListener('click', async () => {
            settings.device = device.id;
            await persist();
            renderDevices();
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
        await persist();
        renderPresets();
        return;
    }

    try {
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
    persist();
}

async function persist() {
    try {
        await invoke('save_settings', { settings });
    } catch (error) {
        setStatus(`Could not save settings: ${error}`, true);
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
        setStatus('');
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
            ? `<img src="${book.cover}" alt="">`
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

    const destination = await open({
        directory: true,
        multiple: false,
        title: 'Where should the optimized books go?',
    });
    if (!destination) return;

    running = true;
    optimizeBtn.disabled = true;
    optimizeBtn.textContent = 'Processing…';
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

    try {
        const outcomes = await invoke('optimize_books', { jobs, destination, settings });
        showResults(outcomes);
    } catch (error) {
        setStatus(`Processing failed: ${error}`, true);
    } finally {
        running = false;
        optimizeBtn.disabled = false;
        optimizeBtn.textContent = 'Optimize';
    }
}

function showResults(outcomes) {
    resultsSection.hidden = false;
    resultsItems.innerHTML = '';

    for (const outcome of outcomes) {
        const card = document.createElement('div');
        card.className = `result-card ${outcome.error ? 'error' : 'success'}`;

        if (outcome.error) {
            card.innerHTML = `
                <div class="result-header"><span class="filename">${escapeHtml(basename(outcome.path))}</span></div>
                <div class="file-error">${escapeHtml(outcome.error)}</div>`;
        } else {
            const report = outcome.report;
            const change = report.originalSize > 0
                ? (1 - report.optimizedSize / report.originalSize) * 100
                : 0;
            // Dithering to four levels is noise to a DCT codec, so a book of
            // smooth artwork can legitimately come out bigger.
            const label = change < 0 ? 'larger' : 'smaller';

            card.innerHTML = `
                <div class="result-header"><span class="filename">${escapeHtml(report.outputFilename)}</span></div>
                <div class="result-size">
                    <span class="size-original">${formatBytes(report.originalSize)}</span>
                    <span class="size-arrow">&rarr;</span>
                    <span class="size-new">${formatBytes(report.optimizedSize)}</span>
                    <span class="size-reduction">${Math.abs(change).toFixed(1)}% ${label}</span>
                </div>
                <div class="result-summary">${escapeHtml(outcome.summary)}</div>`;
        }

        resultsItems.appendChild(card);
    }

    resultsSection.scrollIntoView({ behavior: 'smooth', block: 'start' });
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
    listen('tauri://drag-enter', () => dropZone.classList.add('dragover'));
    listen('tauri://drag-leave', () => dropZone.classList.remove('dragover'));
    listen('tauri://drag-drop', () => dropZone.classList.remove('dragover'));

    listen('progress', ({ payload }) => {
        const item = progressItems.querySelector(`[data-path="${cssEscape(payload.path)}"]`);
        if (!item) return;
        item.querySelector('.progress-bar').style.width = `${payload.percent}%`;
        item.querySelector('.progress-message').textContent = payload.message;
    });

    listen('finished', ({ payload }) => {
        const item = progressItems.querySelector(`[data-path="${cssEscape(payload.path)}"]`);
        if (!item) return;
        item.querySelector('.progress-bar').classList.add(payload.error ? 'error' : 'complete');
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

    savePresetBtn.addEventListener('click', async () => {
        const name = prompt('Name this preset');
        if (!name) return;
        try {
            applySettings(await invoke('save_preset', { name, settings }));
            setStatus(`Saved "${name}".`);
        } catch (error) {
            setStatus(`Could not save that preset: ${error}`, true);
        }
    });

    deletePresetBtn.addEventListener('click', async () => {
        const id = settings.active;
        try {
            applySettings(await invoke('delete_preset', { id }));
            setStatus('Preset deleted.');
        } catch (error) {
            setStatus(`Could not delete that preset: ${error}`, true);
        }
    });

    optimizeBtn.addEventListener('click', optimize);
}

// ------------------------------------------------------------------ helpers

function setStatus(message, isError = false) {
    statusLine.textContent = message;
    statusLine.classList.toggle('error-text', isError);
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
