/**
 * artifact-preview.js
 * Framework-free drop-in document-preview integration component for Branchyard.
 * Exposes window.BranchyardArtifactPreview with mount(container, options).
 */

(function () {
  'use strict';

  function isSafeBasename(filename) {
    if (typeof filename !== 'string' || filename.length === 0) return false;
    // Disallow path traversal, slashes, backslashes, control characters
    if (filename.includes('/') || filename.includes('\\') || filename.includes('..')) {
      return false;
    }
    // Allow standard alphanumeric + . - _
    return /^[a-zA-Z0-9_\-\.]+$/.test(filename);
  }

  var MAX_RETRIES = 5;

  // What a refused or failed preview means to the person reading it, by the
  // server's error code. `server` shows the server's own message and next
  // step; `operator` keeps them under a disclosure, since they are setup
  // instructions for whoever runs the server; `retry` offers Try again.
  var FAILURES = {
    preview_unsupported: { title: 'No preview for this format', hint: 'Download the original to open it on this device.' },
    preview_unavailable: { title: 'This server cannot make previews right now', hint: 'Download the original to open it on this device.', operator: true },
    conversion_failed: { title: 'This file could not be converted for preview', server: true },
    preview_timeout: { title: 'The preview took too long to make', hint: 'Try again, or download the original.', retry: true },
    preview_too_large: { title: 'This file is too large to preview', server: true },
    preview_output_limit: { title: 'This preview is too large to show', hint: 'Download the original to open it on this device.' },
    preview_busy: { title: 'The server is busy making other previews', hint: 'Try again in a moment.', retry: true },
    not_found: { title: 'This file is no longer available', server: true },
    unknown_preview_asset: { title: 'This page of the preview is no longer available', retry: true },
    unreachable: { title: 'The server cannot be reached', hint: 'Check the connection, then try again.', retry: true },
    error: { title: 'The preview could not be shown', server: true, retry: true }
  };

  // The server's answer as preview data, or as a failure with the server's
  // words. A refusal is a JSON error envelope; a conversion that failed, or
  // whose renderer lacks a component, is a 200 whose status says so.
  function outcome(status, body) {
    var error = (body && body.error) || {};
    if (status === 200 && body && body.status === 'passed') return { data: body };
    if (status === 200 && body && (body.status === 'failed' || body.status === 'unavailable')) {
      return { code: body.status === 'failed' ? 'conversion_failed' : 'preview_unavailable', message: error.message, next: error.next };
    }
    var detail = error.detail || {};
    return { code: error.code || 'error', message: error.message || 'HTTP ' + status, next: detail.next || error.next, retryAfter: detail.retry_after };
  }

  function mount(container, options) {
    if (!container) {
      throw new Error('mount requires a valid DOM container element');
    }
    options = options || {};
    var baseUrl = options.baseUrl;
    var fetchAuthenticated = options.fetchAuthenticated;
    var displayName = options.displayName || '';
    var mediaType = options.mediaType || '';
    var viewer = options.viewer === true;

    if (typeof baseUrl !== 'string' || typeof fetchAuthenticated !== 'function') {
      throw new Error('mount options require baseUrl (string) and fetchAuthenticated (function)');
    }

    // Instance state
    var isDisposed = false;
    var activeAbortControllers = new Set();
    var retryTimerId = null;
    var activeBlobUrls = [];
    var generation = 0;

    // View state
    var currentView = viewer && /wordprocessingml|presentationml|application\/pdf/.test(mediaType) ? 'pages' : 'auto'; // 'auto' | 'table' | 'pages'
    var currentPage = 1;
    var currentSheet = '';
    var currentOffset = 0;
    var limit = viewer ? 200 : 100;
    var pageObserver = null;
    var currentDocument = null;
    var pageImages = new Map();
    var pageQueue = Promise.resolve();
    var goToPage = null;

    // Component DOM structure
    var rootEl = document.createElement('div');
    rootEl.className = 'by-artifact-preview' + (viewer ? ' by-document-preview' : '');

    var liveRegion = document.createElement('div');
    liveRegion.className = 'by-preview-sr-only';
    liveRegion.setAttribute('aria-live', 'polite');
    liveRegion.setAttribute('aria-atomic', 'true');
    rootEl.appendChild(liveRegion);

    var toolbarEl = document.createElement('div');
    toolbarEl.className = 'by-preview-toolbar';
    toolbarEl.setAttribute('role', 'toolbar');
    toolbarEl.setAttribute('aria-label', 'Document preview controls');
    rootEl.appendChild(toolbarEl);

    var warningsEl = document.createElement('div');
    warningsEl.className = 'by-preview-warning-banner';
    warningsEl.style.display = 'none';
    rootEl.appendChild(warningsEl);

    var contentEl = document.createElement('div');
    contentEl.className = 'by-preview-content';
    contentEl.setAttribute('tabindex', '0');
    contentEl.setAttribute('aria-label', 'Preview content');
    rootEl.appendChild(contentEl);

    container.appendChild(rootEl);

    function announce(msg) {
      if (!isDisposed && liveRegion) {
        liveRegion.textContent = '';
        if (typeof queueMicrotask === 'function') {
          queueMicrotask(function () {
            if (!isDisposed && liveRegion) {
              liveRegion.textContent = msg;
            }
          });
        } else {
          liveRegion.textContent = msg;
        }
      }
    }

    function revokeAllBlobs() {
      for (var i = 0; i < activeBlobUrls.length; i++) {
        try {
          URL.revokeObjectURL(activeBlobUrls[i]);
        } catch (e) {
          // ignore
        }
      }
      activeBlobUrls = [];
    }

    function cancelPendingRequests() {
      if (pageObserver) pageObserver.disconnect();
      pageObserver = null;
      goToPage = null;
      currentDocument = null;
      pageImages.clear();
      pageQueue = Promise.resolve();
      activeAbortControllers.forEach(function (ctrl) {
        try {
          ctrl.abort();
        } catch (e) {
          // ignore
        }
      });
      activeAbortControllers.clear();

      if (retryTimerId !== null) {
        clearTimeout(retryTimerId);
        retryTimerId = null;
      }
    }

    function renderToolbar(data) {
      toolbarEl.textContent = '';

      var leftGroup = document.createElement('div');
      leftGroup.className = 'by-preview-toolbar-left';

      var rightGroup = document.createElement('div');
      rightGroup.className = 'by-preview-toolbar-right';

      // Title & Format Badge
      if (displayName) {
        var titleEl = document.createElement('span');
        titleEl.className = 'by-preview-title';
        titleEl.textContent = displayName;
        titleEl.title = displayName;
        leftGroup.appendChild(titleEl);
      }

      var effectiveFormat = (data && data.format) || mediaType;
      if (effectiveFormat) {
        var badgeEl = document.createElement('span');
        badgeEl.className = 'by-preview-badge';
        badgeEl.textContent = effectiveFormat;
        leftGroup.appendChild(badgeEl);
      }

      // If data is XLSX or doc supports view switching
      var isXlsx = (data && data.format === 'xlsx') || (mediaType.indexOf('spreadsheet') !== -1 || mediaType.indexOf('excel') !== -1);
      if (isXlsx && data && (data.kind === 'table' || data.kind === 'pages')) {
        var switchGroup = document.createElement('div');
        switchGroup.className = 'by-preview-switch';
        switchGroup.setAttribute('role', 'group');
        switchGroup.setAttribute('aria-label', 'View mode');

        var tableBtn = document.createElement('button');
        tableBtn.type = 'button';
        tableBtn.className = 'by-preview-switch-btn' + (data.kind === 'table' ? ' active' : '');
        tableBtn.textContent = 'Table';
        tableBtn.setAttribute('aria-pressed', data.kind === 'table' ? 'true' : 'false');
        tableBtn.addEventListener('click', function () {
          if (currentView !== 'table') {
            currentView = 'table';
            currentOffset = 0;
            currentPage = 1;
            loadPreview();
          }
        });

        var printBtn = document.createElement('button');
        printBtn.type = 'button';
        printBtn.className = 'by-preview-switch-btn' + (data.kind === 'pages' ? ' active' : '');
        printBtn.textContent = 'Print View';
        printBtn.setAttribute('aria-pressed', data.kind === 'pages' ? 'true' : 'false');
        printBtn.addEventListener('click', function () {
          if (currentView !== 'pages') {
            currentView = 'pages';
            currentPage = 1;
            currentOffset = 0;
            loadPreview();
          }
        });

        switchGroup.appendChild(tableBtn);
        switchGroup.appendChild(printBtn);
        leftGroup.appendChild(switchGroup);
      }

      // Sheet selector if sheets exist
      if (data && data.kind === 'table' && Array.isArray(data.sheets) && data.sheets.length > 1) {
        var sheetLabel = document.createElement('label');
        sheetLabel.className = 'by-preview-sr-only';
        sheetLabel.textContent = 'Select Sheet';
        var sheetSelect = document.createElement('select');
        sheetSelect.className = 'by-preview-select';
        sheetSelect.setAttribute('aria-label', 'Select sheet');
        for (var i = 0; i < data.sheets.length; i++) {
          var opt = document.createElement('option');
          opt.value = data.sheets[i];
          opt.textContent = data.sheets[i];
          if (data.sheets[i] === (data.sheet || currentSheet)) {
            opt.selected = true;
          }
          sheetSelect.appendChild(opt);
        }
        sheetSelect.addEventListener('change', function () {
          currentSheet = sheetSelect.value;
          currentOffset = 0;
          loadPreview();
        });
        if (!viewer) leftGroup.appendChild(sheetSelect);
        else {
          var tabs = document.createElement('div');
          tabs.className = 'by-preview-sheet-tabs';
          tabs.setAttribute('role', 'tablist');
          tabs.setAttribute('aria-label', 'Worksheets');
          data.sheets.forEach(function (name) {
            var tab = document.createElement('button');
            tab.type = 'button';
            tab.className = 'by-preview-btn';
            tab.setAttribute('role', 'tab');
            tab.setAttribute('aria-selected', String(name === data.sheet));
            tab.textContent = name;
            tab.addEventListener('click', function () { currentSheet = name; currentOffset = 0; loadPreview(); });
            tabs.appendChild(tab);
          });
          leftGroup.appendChild(tabs);
        }
      }

      // Page navigation for pages kind
      if (data && data.kind === 'pages') {
        var pageCount = typeof data.pages === 'number' ? data.pages : 1;
        var activePage = typeof data.page === 'number' ? data.page : currentPage;

        var prevBtn = document.createElement('button');
        prevBtn.type = 'button';
        prevBtn.className = 'by-preview-btn';
        prevBtn.textContent = '◀ Prev';
        prevBtn.setAttribute('aria-label', 'Previous page');
        prevBtn.disabled = activePage <= 1;
        prevBtn.addEventListener('click', function () {
          if (activePage > 1) {
            if (viewer && goToPage) goToPage(activePage - 1);
            else { currentPage = activePage - 1; loadPreview(); }
          }
        });

        var pageLabel = document.createElement('span');
        pageLabel.className = 'by-preview-page-indicator';
        pageLabel.textContent = activePage + ' / ' + pageCount;

        var nextBtn = document.createElement('button');
        nextBtn.type = 'button';
        nextBtn.className = 'by-preview-btn';
        nextBtn.textContent = 'Next ▶';
        nextBtn.setAttribute('aria-label', 'Next page');
        nextBtn.disabled = activePage >= pageCount;
        nextBtn.addEventListener('click', function () {
          if (activePage < pageCount) {
            if (viewer && goToPage) goToPage(activePage + 1);
            else { currentPage = activePage + 1; loadPreview(); }
          }
        });

        if (viewer) {
          var zoom = document.createElement('select');
          zoom.className = 'by-preview-select';
          zoom.setAttribute('aria-label', 'Zoom');
          [['fit', 'Fit to width'], ['75', '75%'], ['100', '100%'], ['125', '125%'], ['150', '150%']].forEach(function (entry) {
            var option = document.createElement('option');
            option.value = entry[0]; option.textContent = entry[1];
            zoom.appendChild(option);
          });
          zoom.value = rootEl.dataset.zoom || 'fit';
          zoom.addEventListener('change', function () {
            rootEl.dataset.zoom = zoom.value;
            rootEl.style.setProperty('--by-document-width', zoom.value === 'fit' ? '100%' : zoom.value + '%');
          });
          leftGroup.appendChild(zoom);
        }
        rightGroup.appendChild(prevBtn);
        rightGroup.appendChild(pageLabel);
        rightGroup.appendChild(nextBtn);
      }

      toolbarEl.appendChild(leftGroup);
      toolbarEl.appendChild(rightGroup);
    }

    function renderWarnings(warnings) {
      if (Array.isArray(warnings) && warnings.length > 0) {
        warningsEl.textContent = '';
        var warnText = warnings.join('; ');
        var iconSpan = document.createElement('span');
        iconSpan.setAttribute('aria-hidden', 'true');
        iconSpan.textContent = '⚠️ ';
        warningsEl.appendChild(iconSpan);

        var msgSpan = document.createElement('span');
        msgSpan.textContent = warnText;
        warningsEl.appendChild(msgSpan);

        warningsEl.style.display = 'flex';
      } else {
        warningsEl.style.display = 'none';
        warningsEl.textContent = '';
      }
    }

    function renderLoading(msg) {
      contentEl.textContent = '';
      renderWarnings([]);

      var container = document.createElement('div');
      container.className = 'by-preview-state-container';

      var spinner = document.createElement('div');
      spinner.className = 'by-preview-spinner';
      spinner.setAttribute('aria-hidden', 'true');
      container.appendChild(spinner);

      var text = document.createElement('div');
      text.className = 'by-preview-state-message';
      text.textContent = msg || 'Making a preview…';
      container.appendChild(text);

      contentEl.appendChild(container);
      announce(text.textContent);
    }

    function textBlock(tag, className, text) {
      var el = document.createElement(tag);
      el.className = className;
      el.textContent = text;
      return el;
    }

    // One failure as `outcome` reads it: a plain title, then the server's
    // words where they help the reader, or under a disclosure for the
    // server's operator.
    function renderFailure(failure) {
      var meaning = FAILURES[failure.code] || FAILURES.error;
      contentEl.textContent = '';
      toolbarEl.textContent = '';
      renderWarnings([]);

      var container = document.createElement('div');
      container.className = 'by-preview-state-container by-preview-failure';
      container.appendChild(textBlock('p', 'by-preview-state-message', meaning.title));
      if (meaning.server && failure.message) container.appendChild(textBlock('p', 'by-preview-state-subtext', failure.message));
      if (meaning.server && failure.next) container.appendChild(textBlock('p', 'by-preview-state-subtext', 'Next: ' + failure.next));
      if (meaning.hint) container.appendChild(textBlock('p', 'by-preview-state-subtext', meaning.hint));
      if (meaning.operator && (failure.message || failure.next)) {
        var details = document.createElement('details');
        details.className = 'by-preview-operator';
        details.appendChild(textBlock('summary', '', 'Details for the server\'s operator'));
        if (failure.message) details.appendChild(textBlock('p', '', failure.message));
        if (failure.next) details.appendChild(textBlock('p', '', failure.next));
        container.appendChild(details);
      }
      if (meaning.retry) {
        var retryBtn = document.createElement('button');
        retryBtn.type = 'button';
        retryBtn.className = 'by-preview-btn';
        retryBtn.textContent = 'Try again';
        retryBtn.addEventListener('click', function () {
          loadPreview();
        });
        container.appendChild(retryBtn);
      }
      contentEl.appendChild(container);
      announce(meaning.title);
    }

    function renderTable(data) {
      contentEl.textContent = '';
      renderToolbar(data);
      renderWarnings(data.warnings);

      var tableContainer = document.createElement('div');
      tableContainer.className = 'by-preview-table-container';

      var table = document.createElement('table');
      table.className = 'by-preview-table';
      table.setAttribute('role', 'table');
      if (viewer && data.format === 'xlsx') {
        table.classList.add('by-preview-sheet-grid');
        table.classList.toggle('by-preview-frozen-header', data.frozen_rows > 0);
        var colgroup = document.createElement('colgroup');
        (data.column_widths || []).forEach(function (width) {
          var col = document.createElement('col');
          if (typeof width === 'number' && isFinite(width)) col.style.width = Math.max(16, Math.min(2000, width)) + 'px';
          colgroup.appendChild(col);
        });
        table.appendChild(colgroup);
      }
      function fillCell(cell, value, detail, row, col) {
        cell.textContent = detail && detail.formatted != null ? String(detail.formatted) : (value != null ? String(value) : '');
        if (detail && detail.formula) cell.title = String(detail.formula);
        var merges = viewer && Array.isArray(data.merges) ? data.merges : [];
        for (var m = 0; m < merges.length; m++) {
          var merge = merges[m];
          if (row < merge.min_row || row > merge.max_row || col < merge.min_col || col > merge.max_col) continue;
          if (row !== merge.min_row || col !== merge.min_col) return false;
          // Do not merge through the independently frozen header or outside this row window.
          cell.colSpan = Math.min(100, merge.max_col - merge.min_col + 1);
          cell.rowSpan = row === 1 ? 1 : Math.min(merge.max_row - row + 1, (data.offset || 0) + data.rows.length + 2 - row);
        }
        return true;
      }

      var thead = document.createElement('thead');
      var headerRow = document.createElement('tr');
      var columns = Array.isArray(data.columns) ? data.columns : [];

      for (var c = 0; c < columns.length; c++) {
        var th = document.createElement('th');
        th.setAttribute('scope', 'col');
        if (fillCell(th, columns[c], (data.header_details || [])[c], 1, c + 1)) headerRow.appendChild(th);
      }
      thead.appendChild(headerRow);
      table.appendChild(thead);

      var tbody = document.createElement('tbody');
      var rows = Array.isArray(data.rows) ? data.rows : [];

      for (var r = 0; r < rows.length; r++) {
        var tr = document.createElement('tr');
        var rowData = rows[r];
        if (Array.isArray(rowData)) {
          for (var i = 0; i < rowData.length; i++) {
            var td = document.createElement('td');
            // Strict inert textContent - prevent CSV injection formula / script execution
            if (fillCell(td, rowData[i], ((data.cell_details || [])[r] || [])[i], (data.offset || 0) + r + 2, i + 1)) tr.appendChild(td);
          }
        }
        tbody.appendChild(tr);
      }
      table.appendChild(tbody);
      tableContainer.appendChild(table);
      contentEl.appendChild(tableContainer);

      // Pagination footer if offset or next_offset
      var hasPagination = (typeof data.offset === 'number' && data.offset > 0) || (typeof data.next_offset === 'number');
      if (hasPagination || (viewer && data.kind === 'table')) {
        var footer = document.createElement('div');
        footer.className = 'by-preview-table-footer';

        var rowsInfo = document.createElement('span');
        var startRow = (data.offset || 0) + (viewer && data.format === 'xlsx' ? 2 : 1);
        var endRow = startRow + rows.length - 1;
        rowsInfo.textContent = rows.length > 0 ? ('Showing rows ' + startRow + '–' + endRow) : 'No rows';
        footer.appendChild(rowsInfo);

        var pageNav = document.createElement('div');
        pageNav.className = 'by-preview-toolbar-right';

        var prevPageBtn = document.createElement('button');
        prevPageBtn.type = 'button';
        prevPageBtn.className = 'by-preview-btn';
        prevPageBtn.textContent = '◀ Previous ' + limit;
        prevPageBtn.disabled = !data.offset || data.offset <= 0;
        prevPageBtn.addEventListener('click', function () {
          currentOffset = Math.max(0, (data.offset || 0) - limit);
          loadPreview();
        });
        pageNav.appendChild(prevPageBtn);

        var nextPageBtn = document.createElement('button');
        nextPageBtn.type = 'button';
        nextPageBtn.className = 'by-preview-btn';
        nextPageBtn.textContent = 'Next ' + limit + ' ▶';
        nextPageBtn.disabled = typeof data.next_offset !== 'number';
        nextPageBtn.addEventListener('click', function () {
          if (typeof data.next_offset === 'number') {
            currentOffset = data.next_offset;
            loadPreview();
          }
        });
        pageNav.appendChild(nextPageBtn);

        footer.appendChild(pageNav);
        contentEl.appendChild(footer);
      }

      announce('Preview ready: ' + rows.length + (rows.length === 1 ? ' row.' : ' rows.'));
    }

    function renderPages(data, currentGen, previewQuery) {
      if (viewer) { renderDocumentPages(data, currentGen); return; }
      renderToolbar(data);
      renderWarnings(data.warnings);
      contentEl.textContent = '';

      var imageFilename = data.image;
      if (!imageFilename || !isSafeBasename(imageFilename)) {
        renderFailure({ code: 'error', message: 'The page image has an unexpected name.' });
        return;
      }

      var pagesContainer = document.createElement('div');
      pagesContainer.className = 'by-preview-pages-container';

      var spinner = document.createElement('div');
      spinner.className = 'by-preview-spinner';
      pagesContainer.appendChild(spinner);
      contentEl.appendChild(pagesContainer);

      // Fetch the asset with the SAME preview query and authenticated fetch
      var assetPath = baseUrl + '/preview/assets/' + encodeURIComponent(imageFilename) + previewQuery;
      var assetAbort = new AbortController();
      activeAbortControllers.add(assetAbort);

      fetchAuthenticated(assetPath, {
        method: 'GET',
        signal: assetAbort.signal
      }).then(function (res) {
        activeAbortControllers.delete(assetAbort);
        if (currentGen !== generation || isDisposed) return;
        if (!res.ok) {
          throw new Error('the server answered ' + res.status + '.');
        }
        var ctype = res.headers && res.headers.get ? (res.headers.get('content-type') || '') : '';
        if (ctype.toLowerCase().indexOf('image/png') === -1) {
          throw new Error('it is not a PNG image.');
        }
        return res.blob();
      }).then(function (blob) {
        if (currentGen !== generation || isDisposed || !blob) return;
        var blobUrl = URL.createObjectURL(blob);
        activeBlobUrls.push(blobUrl);

        pagesContainer.textContent = '';
        var img = document.createElement('img');
        img.className = 'by-preview-page-img';
        img.src = blobUrl;
        img.alt = 'Page ' + (data.page || currentPage) + ' of ' + (data.pages || 1);
        if (typeof data.width === 'number' && data.width > 0) {
          img.style.maxWidth = data.width + 'px';
        }
        pagesContainer.appendChild(img);
        announce('Preview ready: page ' + (data.page || currentPage) + ' of ' + (data.pages || 1) + '.');
      }).catch(function (err) {
        activeAbortControllers.delete(assetAbort);
        if (currentGen !== generation || isDisposed) return;
        if (err.name === 'AbortError') return;
        pagesContainer.textContent = '';
        var errBox = document.createElement('div');
        errBox.className = 'by-preview-state-message';
        errBox.textContent = 'The page image did not load: ' + err.message;
        pagesContainer.appendChild(errBox);
        announce(errBox.textContent);
      });
    }

    // Each visible page enters one serial queue. The service caches derivatives
    // per artifact and page; an instance also shares the image between slide and thumbnail.
    function pageImage(number, first, gen) {
      if (pageImages.has(number)) return pageImages.get(number);
      var result = pageQueue.then(function () {
        if (gen !== generation || isDisposed) return null;
        var query = '?view=pages&page=' + number + (currentSheet ? '&sheet=' + encodeURIComponent(currentSheet) : '') + '&limit=' + limit;
        var ctrl = new AbortController();
        activeAbortControllers.add(ctrl);
        var metadata = number === first.page ? Promise.resolve({ data: first }) : fetchAuthenticated(baseUrl + '/preview' + query, { signal: ctrl.signal }).then(function (res) {
          return res.json().then(function (body) { return outcome(res.status, body); });
        });
        return metadata.then(function (answer) {
          if (!answer.data) throw new Error(answer.message || (FAILURES[answer.code] || FAILURES.error).title);
          var page = answer.data;
          if (!isSafeBasename(page.image)) throw new Error('The page image has an unexpected name.');
          return fetchAuthenticated(baseUrl + '/preview/assets/' + encodeURIComponent(page.image) + query, { signal: ctrl.signal });
        }).then(function (res) {
          if (!res.ok || (res.headers.get('content-type') || '').toLowerCase().indexOf('image/png') === -1) throw new Error('The page image did not load.');
          return res.blob();
        }).then(function (blob) {
          if (gen !== generation || isDisposed) return null;
          var url = URL.createObjectURL(blob);
          activeBlobUrls.push(url);
          return url;
        }).finally(function () { activeAbortControllers.delete(ctrl); });
      });
      // A refused page does not prevent another page from being requested.
      pageQueue = result.catch(function () {});
      pageImages.set(number, result);
      return result;
    }

    function renderDocumentPages(data, gen) {
      currentDocument = data;
      currentPage = data.page || 1;
      var slides = data.format === 'pptx';
      var count = Math.min(64, Math.max(1, data.pages || 1));
      contentEl.textContent = '';
      contentEl.classList.toggle('by-preview-slide-content', slides);
      renderWarnings(data.warnings);
      var canvas = document.createElement('div');
      canvas.className = slides ? 'by-preview-slide-stage' : 'by-preview-page-stack';
      var slots = [];
      function putImage(slot, number, thumbnail) {
        if (slot.dataset.loading) return;
        slot.dataset.loading = 'true';
        pageImage(number, data, gen).then(function (url) {
          if (!url || gen !== generation || isDisposed || (slot.classList.contains('by-preview-slide') && Number(slot.dataset.page) !== number)) return;
          slot.textContent = '';
          var img = document.createElement('img');
          img.src = url;
          img.alt = (slides ? 'Slide ' : 'Page ') + number + ' of ' + count;
          img.className = thumbnail ? 'by-preview-thumbnail-img' : 'by-preview-page-img';
          slot.appendChild(img);
          if (!thumbnail) announce('Preview ready: ' + (slides ? 'slide ' : 'page ') + number + ' of ' + count + '.');
        }).catch(function (error) {
          if (gen !== generation || isDisposed || error.name === 'AbortError') return;
          slot.textContent = error.message;
          delete slot.dataset.loading;
          var retry = document.createElement('button');
          retry.className = 'by-preview-btn'; retry.textContent = 'Try again';
          retry.addEventListener('click', function () { pageImages.delete(number); slot.textContent = ''; putImage(slot, number, thumbnail); });
          slot.appendChild(retry);
        });
      }
      if (slides) {
        var folded = document.createElement('details');
        folded.className = 'by-preview-thumbnails';
        folded.open = window.matchMedia('(min-width: 601px)').matches;
        var summary = document.createElement('summary');
        summary.textContent = 'Slides';
        folded.appendChild(summary);
        var strip = document.createElement('div');
        strip.className = 'by-preview-thumbnail-strip';
        strip.setAttribute('aria-label', 'Slide thumbnails');
        folded.appendChild(strip);
        contentEl.appendChild(folded);
        var stage = document.createElement('div');
        stage.className = 'by-preview-slide';
        canvas.appendChild(stage);
        goToPage = function (number) {
          currentPage = Math.max(1, Math.min(count, number));
          stage.textContent = ''; delete stage.dataset.loading;
          stage.dataset.page = String(currentPage);
          strip.querySelectorAll('button').forEach(function (button) { button.setAttribute('aria-current', String(Number(button.dataset.page) === currentPage)); });
          renderToolbar(Object.assign({}, data, { page: currentPage }));
          putImage(stage, currentPage, false);
        };
        for (var n = 1; n <= count; n++) {
          var button = document.createElement('button');
          button.type = 'button'; button.className = 'by-preview-thumbnail';
          button.dataset.page = String(n);
          button.setAttribute('aria-label', 'Slide ' + n);
          var thumb = document.createElement('span');
          thumb.className = 'by-preview-thumbnail-image';
          thumb.dataset.page = String(n);
          button.appendChild(thumb);
          var label = document.createElement('span'); label.textContent = String(n); button.appendChild(label);
          button.addEventListener('click', function (event) { goToPage(Number(event.currentTarget.dataset.page)); });
          strip.appendChild(button); slots.push(thumb);
        }
        contentEl.appendChild(canvas);
        pageObserver = new IntersectionObserver(function (entries) {
          entries.forEach(function (entry) { if (entry.isIntersecting) putImage(entry.target, Number(entry.target.dataset.page), true); });
        }, { root: strip, rootMargin: '100px' });
        slots.forEach(function (slot) { pageObserver.observe(slot); });
        goToPage(currentPage);
      } else {
        for (var p = 1; p <= count; p++) {
          var slot = document.createElement('section');
          slot.className = 'by-preview-document-page';
          slot.dataset.page = String(p);
          slot.setAttribute('aria-label', 'Page ' + p);
          if (data.width > 0 && data.height > 0) slot.style.aspectRatio = data.width + ' / ' + data.height;
          slot.textContent = 'Page ' + p;
          slots.push(slot); canvas.appendChild(slot);
        }
        contentEl.appendChild(canvas);
        goToPage = function (number) {
          number = Math.max(1, Math.min(count, number));
          slots[number - 1].scrollIntoView({ block: 'start' });
          currentPage = number;
          renderToolbar(Object.assign({}, data, { page: currentPage }));
          putImage(slots[number - 1], number, false);
        };
        pageObserver = new IntersectionObserver(function (entries) {
          entries.forEach(function (entry) { if (entry.isIntersecting) putImage(entry.target, Number(entry.target.dataset.page), false); });
        }, { root: contentEl, rootMargin: '100px' });
        slots.forEach(function (slot) { pageObserver.observe(slot); });
        renderToolbar(data);
        putImage(slots[currentPage - 1], currentPage, false);
      }
    }

    function viewerKey(event) {
      if (!viewer || !goToPage || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.target.closest('input, select, textarea, [contenteditable]')) return;
      var delta = ['ArrowRight', 'ArrowDown', 'PageDown'].includes(event.key) ? 1 : ['ArrowLeft', 'ArrowUp', 'PageUp'].includes(event.key) ? -1 : 0;
      if (delta) { event.preventDefault(); goToPage(currentPage + delta); }
      else if (event.key === 'Home' || event.key === 'End') { event.preventDefault(); goToPage(event.key === 'Home' ? 1 : currentDocument.pages); }
    }
    if (viewer) {
      document.addEventListener('keydown', viewerKey);
      contentEl.addEventListener('scroll', function () {
        if (!currentDocument || currentDocument.format === 'pptx') return;
        var viewport = contentEl.getBoundingClientRect();
        var visible = null;
        var largestOverlap = 0;
        contentEl.querySelectorAll('.by-preview-document-page').forEach(function (slot) {
          var bounds = slot.getBoundingClientRect();
          var overlap = Math.max(0, Math.min(bounds.bottom, viewport.bottom) - Math.max(bounds.top, viewport.top));
          if (overlap > largestOverlap) { visible = slot; largestOverlap = overlap; }
        });
        if (visible && Number(visible.dataset.page) !== currentPage) {
          currentPage = Number(visible.dataset.page);
          renderToolbar(Object.assign({}, currentDocument, { page: currentPage }));
        }
      });
    }

    function renderText(data) {
      contentEl.textContent = '';
      renderToolbar(data);
      renderWarnings(data.warnings);

      var textContainer = document.createElement('div');
      textContainer.className = 'by-preview-text-container';

      var pre = document.createElement('pre');
      pre.className = 'by-preview-text-content';
      pre.textContent = typeof data.text === 'string' ? data.text : '';
      textContainer.appendChild(pre);

      contentEl.appendChild(textContainer);
      announce('Preview ready.');
    }

    function loadPreview() {
      if (isDisposed) return;

      cancelPendingRequests();
      revokeAllBlobs();

      var thisGen = ++generation;
      renderLoading();

      var queryParts = ['view=' + encodeURIComponent(currentView)];
      if (currentPage > 1 || viewer) {
        queryParts.push('page=' + encodeURIComponent(currentPage));
      }
      if (currentSheet) {
        queryParts.push('sheet=' + encodeURIComponent(currentSheet));
      }
      if (currentOffset > 0) {
        queryParts.push('offset=' + encodeURIComponent(currentOffset));
      }
      if (limit !== 100) {
        queryParts.push('limit=' + encodeURIComponent(limit));
      }
      var previewQuery = '?' + queryParts.join('&');
      var previewUrl = baseUrl + '/preview' + previewQuery;
      var retries = 0;

      function executeFetch() {
        if (thisGen !== generation || isDisposed) return;

        var reqAbort = new AbortController();
        activeAbortControllers.add(reqAbort);

        fetchAuthenticated(previewUrl, {
          method: 'GET',
          signal: reqAbort.signal
        }).then(function (res) {
          return res.json().catch(function () { return null; }).then(function (body) {
            return outcome(res.status, body);
          });
        }, function (err) {
          if (err.name === 'AbortError') throw err;
          return { code: 'unreachable' };
        }).then(function (result) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed) return;

          // Both of the server's conversion slots are taken; it says when to ask again.
          var wait = result.retryAfter;
          if (result.code === 'preview_busy' && typeof wait === 'number' && wait > 0 && wait <= 10 && retries < MAX_RETRIES) {
            retries++;
            renderLoading('The server is busy making other previews. Trying again in ' + wait + (wait === 1 ? ' second…' : ' seconds…'));
            retryTimerId = setTimeout(function () {
              retryTimerId = null;
              executeFetch();
            }, wait * 1000);
            return;
          }
          if (!result.data) {
            renderFailure(result);
            return;
          }

          var payload = result.data;
          if (payload.kind === 'table') {
            if (payload.sheet) {
              currentSheet = payload.sheet;
            }
            renderTable(payload);
          } else if (payload.kind === 'pages') {
            renderPages(payload, thisGen, previewQuery);
          } else if (payload.kind === 'text') {
            renderText(payload);
          } else {
            renderFailure({ code: 'error', message: 'The server sent a kind of preview this page does not show: ' + payload.kind + '.' });
          }
        }).catch(function (err) {
          activeAbortControllers.delete(reqAbort);
          if (thisGen !== generation || isDisposed || err.name === 'AbortError') return;
          renderFailure({ code: 'error', message: err.message });
        });
      }

      executeFetch();
    }

    // Initial load
    loadPreview();

    return {
      dispose: function () {
        if (isDisposed) return;
        isDisposed = true;
        if (viewer) document.removeEventListener('keydown', viewerKey);
        generation++;
        cancelPendingRequests();
        revokeAllBlobs();
        if (rootEl && rootEl.parentNode) {
          rootEl.parentNode.removeChild(rootEl);
        }
      },
      refresh: function () {
        if (isDisposed) return;
        // Preserves current page / sheet / view on refresh
        loadPreview();
      }
    };
  }

  window.BranchyardArtifactPreview = {
    mount: mount
  };
})();
