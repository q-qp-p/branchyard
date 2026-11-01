"""Bounded local previews for tables, documents, presentations, PDFs, images and text."""

from __future__ import annotations

import csv
import datetime
import io
import json
import os
import re
import stat
import tempfile
import zipfile
from collections.abc import Sequence
from decimal import Decimal
from itertools import zip_longest
from pathlib import Path
from typing import Any

from .common import ArtifactError, Unavailable, finite, library, output_file
from .native import (
    MAX_RENDER_DIMENSION,
    MAX_RENDER_IMAGE_BYTES,
    MAX_RENDER_PAGES,
    MAX_RENDER_PIXELS,
    RENDER_TIMEOUT_SECONDS,
    recalculate_workbook,
    render_page,
)

MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_OUTPUT_IMAGE_BYTES = MAX_RENDER_IMAGE_BYTES
MAX_ARCHIVE_ENTRIES = 10_000
MAX_ARCHIVE_EXPANDED_BYTES = 128 * 1024 * 1024
MAX_PAGES = MAX_RENDER_PAGES
MAX_IMAGE_DIMENSION = 4096
MAX_PREVIEW_DIMENSION = MAX_RENDER_DIMENSION
MAX_IMAGE_PIXELS = MAX_RENDER_PIXELS
MAX_COLUMNS = 100
MAX_LIMIT = 500
MAX_LAYOUT_MERGES = MAX_COLUMNS * (MAX_LIMIT + 1)
MAX_SCAN_ROWS = 100_000
MAX_CELL_CHARS = 1_000
MAX_TEXT_BYTES = 256 * 1024
MAX_TEXT_CHARS = 65_536
PROCESS_TIMEOUT_SECONDS = RENDER_TIMEOUT_SECONDS
MISSING_FORMULA_CACHE_WARNING = "Some formulas have no cached value; recalculate the original workbook to show them"

MACRO_SUFFIXES = {
    ".docm",
    ".dotm",
    ".xlsm",
    ".xltm",
    ".xlam",
    ".xlsb",
    ".pptm",
    ".potm",
    ".ppam",
    ".ppsm",
    ".sldm",
}
ACTIVE_MARKUP_SUFFIXES = {".svg", ".svgz", ".html", ".htm", ".xhtml", ".xml", ".mhtml"}
IMAGE_FORMATS = {
    ".png": "png",
    ".jpg": "jpeg",
    ".jpeg": "jpeg",
    ".webp": "webp",
    ".gif": "gif",
    ".bmp": "bmp",
    ".tif": "tiff",
    ".tiff": "tiff",
}
TEXT_FORMATS = {
    ".txt": "txt",
    ".md": "markdown",
    ".markdown": "markdown",
    ".json": "json",
}
REQUIRED_OFFICE_PARTS = {
    "docx": "word/document.xml",
    "xlsx": "xl/workbook.xml",
    "pptx": "ppt/presentation.xml",
}


def _warn_once(warnings: list[str], message: str) -> None:
    if message not in warnings:
        warnings.append(message)


def _check_no_symlinks(path: Path, label: str) -> None:
    """Refuse symbolic links on the target path or any ancestor component."""
    raw = Path(path)
    if raw.is_symlink():
        raise ArtifactError(label + " must not be a symbolic link")
    absolute = Path(os.path.abspath(raw))
    for candidate in (absolute, *absolute.parents):
        if candidate.is_symlink():
            raise ArtifactError(label + " must not traverse a symbolic link")


def _validate_paths(source: Path, out: Path) -> tuple[Path, Path]:
    """Validate input and output paths before reading or creating files."""
    source, out = Path(source), Path(out)
    _check_no_symlinks(source, "Preview input")
    _check_no_symlinks(out, "Preview output directory")
    source_abs = Path(os.path.abspath(source))
    out_abs = Path(os.path.abspath(out))
    if out_abs == source_abs or (out.exists() and source.exists() and os.path.samefile(out, source)):
        raise ArtifactError("Preview output directory must not overwrite the source file")
    if out.exists() and not out.is_dir():
        raise ArtifactError("Preview output destination must be a directory")
    return source, out


def _read_regular(path: Path, limit: int = MAX_INPUT_BYTES) -> bytes:
    """Read a bounded regular file without following a final symlink."""
    _check_no_symlinks(path, "Preview input")
    flags = os.O_RDONLY | os.O_NONBLOCK | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ArtifactError("Preview input must be an existing regular file") from error
    with os.fdopen(descriptor, "rb") as stream:
        metadata = os.fstat(stream.fileno())
        if not stat.S_ISREG(metadata.st_mode):
            raise ArtifactError("Preview input must be a regular file")
        if metadata.st_size == 0:
            raise ArtifactError("Preview input is empty")
        if metadata.st_size > limit:
            raise ArtifactError("Preview input exceeds the byte limit")
        payload = stream.read(limit + 1)
    if not payload:
        raise ArtifactError("Preview input is empty")
    if len(payload) > limit:
        raise ArtifactError("Preview input exceeds the byte limit")
    return payload


def _check_office_archive(payload: bytes, fmt: str) -> None:
    """Validate Office ZIP entry count, expanded size, structure and absence of macros."""
    try:
        archive_context = zipfile.ZipFile(io.BytesIO(payload))
    except (zipfile.BadZipFile, OSError, ValueError, EOFError) as error:
        raise ArtifactError("Malformed Office archive") from error
    with archive_context as archive:
        entries = archive.infolist()
        if not entries or len(entries) > MAX_ARCHIVE_ENTRIES:
            raise ArtifactError("Office archive entry count exceeds preview bounds")
        expanded = 0
        names: set[str] = set()
        for info in entries:
            name = info.filename
            parts = Path(name).parts
            if not name or "\x00" in name or name.startswith(("/", "\\")) or ".." in parts:
                raise ArtifactError("Office archive contains an unsafe member path")
            if stat.S_ISLNK(info.external_attr >> 16):
                raise ArtifactError("Office archive contains a symbolic link member")
            if info.flag_bits & 0x1:
                raise ArtifactError("Encrypted Office archive is not supported")
            if info.file_size < 0 or info.file_size > MAX_ARCHIVE_EXPANDED_BYTES:
                raise ArtifactError("Office archive member exceeds expanded byte limit")
            expanded += info.file_size
            if expanded > MAX_ARCHIVE_EXPANDED_BYTES:
                raise ArtifactError("Expanded Office archive exceeds byte limit")
            lowered = name.lower()
            if (
                lowered.endswith(("vbaproject.bin", "vbadata.xml"))
                or "/macros/" in lowered
                or "macrosheets/" in lowered
            ):
                raise ArtifactError("Macro-enabled Office archive is refused")
            names.add(name)
        required_part = REQUIRED_OFFICE_PARTS.get(fmt)
        if "[Content_Types].xml" not in names or (required_part and required_part not in names):
            raise ArtifactError("Malformed " + fmt.upper() + " package: missing required document parts")
        try:
            content_types = archive.read("[Content_Types].xml").decode("utf-8", errors="replace").lower()
        except Exception as error:
            raise ArtifactError("Malformed Office [Content_Types].xml part") from error
        if "vbaproject" in content_types or "macroenabled" in content_types:
            raise ArtifactError("Macro-enabled Office content type is refused")


def _truncate_text(text: str, warnings: list[str]) -> str:
    if len(text) > MAX_CELL_CHARS:
        _warn_once(warnings, "Truncated cell text to " + str(MAX_CELL_CHARS) + " characters")
        return text[:MAX_CELL_CHARS]
    return text


def _trimmed_csv_row(raw_row: list[str], warnings: list[str]) -> list[str]:
    if any(raw_row[MAX_COLUMNS:]):
        _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
    return raw_row[:MAX_COLUMNS]


def _preview_csv(path: Path, offset: int, limit: int) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if b"\x00" in payload:
        raise ArtifactError("CSV input contains NUL bytes")
    try:
        decoded = payload.decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ArtifactError("CSV input must be valid UTF-8") from error
    warnings: list[str] = []
    reader = csv.reader(io.StringIO(decoded, newline=""), strict=True)
    try:
        raw_header = next(reader, None)
    except csv.Error as error:
        raise ArtifactError("Malformed CSV header: " + str(error)) from error
    if raw_header is None or not raw_header or all(cell == "" for cell in raw_header):
        raise ArtifactError("CSV input has no header row")
    raw_header = _trimmed_csv_row(raw_header, warnings)
    columns = [
        _truncate_text(label, warnings) if label != "" else "column_" + str(index + 1)
        for index, label in enumerate(raw_header)
    ]
    rows: list[list[Any]] = []
    next_offset: int | None = None
    row_index = 0
    try:
        for raw_row in reader:
            if row_index >= MAX_SCAN_ROWS:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
                break
            raw_row = _trimmed_csv_row(raw_row, warnings)
            if len(raw_row) > len(columns):
                for extra in range(len(columns), len(raw_row)):
                    columns.append("column_" + str(extra + 1))
            if row_index < offset:
                row_index += 1
                continue
            if len(rows) < limit:
                padded = [*raw_row, *([""] * max(0, len(columns) - len(raw_row)))]
                rows.append([_truncate_text(cell, warnings) for cell in padded[: len(columns)]])
                row_index += 1
                continue
            if offset + len(rows) < MAX_SCAN_ROWS:
                next_offset = offset + len(rows)
            else:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                next_offset = None
            break
    except csv.Error as error:
        raise ArtifactError("Malformed CSV record: " + str(error)) from error
    for row in rows:
        if len(row) < len(columns):
            row.extend([""] * (len(columns) - len(row)))
    return {
        "status": "passed",
        "kind": "table",
        "format": "csv",
        "columns": columns,
        "rows": rows,
        "sheets": [],
        "sheet": None,
        "offset": offset,
        "next_offset": next_offset,
        "warnings": warnings,
    }


def _xlsx_cell(value: Any, warnings: list[str]) -> Any:
    if value is None or isinstance(value, bool):
        return value
    if isinstance(value, int):
        finite(value, "Worksheet cell")
        return value
    if isinstance(value, float):
        return finite(value, "Worksheet cell")
    if isinstance(value, Decimal):
        return finite(float(value), "Worksheet cell")
    if isinstance(value, (datetime.date, datetime.time, datetime.datetime)):
        return value.isoformat()
    formula_text = getattr(value, "text", None)
    if isinstance(formula_text, str):
        _warn_once(warnings, "Workbook formulas are shown as literal formula text and were not evaluated")
        return _truncate_text(formula_text, warnings)
    if isinstance(value, str):
        if value.startswith("="):
            _warn_once(warnings, "Workbook formulas are shown as literal formula text and were not evaluated")
        return _truncate_text(value, warnings)
    return _truncate_text(str(value), warnings)


def _trimmed_xlsx_row(raw_row: Sequence[Any], warnings: list[str]) -> list[Any]:
    overflow = raw_row[MAX_COLUMNS:]
    # count() stays in C; a styled empty cell in column XFD pads the row to 16,384 entries.
    if overflow.count(None) < len(overflow):
        _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
    cells = list(raw_row[:MAX_COLUMNS])
    while cells and cells[-1] is None:
        cells.pop()
    return [_xlsx_cell(cell, warnings) for cell in cells]


def _xlsx_layout(workbook: Any, worksheet: Any, offset: int, limit: int) -> dict[str, Any]:
    """Read bounded layout metadata while keeping worksheet values in streaming mode."""
    xml = library("openpyxl.xml.functions")
    bounds = library("openpyxl.utils.cell").range_boundaries
    widths = [64.0] * MAX_COLUMNS
    merges: list[dict[str, int]] = []
    frozen_rows = 0
    with workbook._archive.open(worksheet._worksheet_path) as source:
        for _, element in xml.iterparse(source, events=("end",)):
            tag = element.tag.rsplit("}", 1)[-1]
            if tag == "sheetFormatPr":
                default = float(element.get("defaultColWidth", "8.43"))
                widths = [min(1000.0, max(8.0, default * 7 + 5))] * MAX_COLUMNS
            elif tag == "col":
                first = max(1, int(element.get("min", "1")))
                last = min(MAX_COLUMNS, int(element.get("max", "1")))
                width = float(element.get("width", "8.43")) * 7 + 5
                width = min(1000.0, max(8.0, width)) if element.get("hidden") != "1" else 0.0
                for index in range(first - 1, last):
                    widths[index] = width
            elif tag == "pane" and element.get("state") in {"frozen", "frozenSplit"}:
                frozen_rows = min(MAX_SCAN_ROWS, max(0, int(float(element.get("ySplit", "0")))))
            elif tag == "mergeCell":
                min_col, min_row, max_col, max_row = bounds(element.get("ref", "A1"))
                visible = min_row <= offset + limit + 1 and max_row >= offset + 2
                if min_col <= MAX_COLUMNS and (visible or min_row == 1):
                    if len(merges) >= MAX_LAYOUT_MERGES:
                        raise ArtifactError("Worksheet merged-cell metadata exceeds preview bounds")
                    merges.append(
                        {
                            "min_row": min_row,
                            "max_row": min(max_row, MAX_SCAN_ROWS + 1),
                            "min_col": min_col,
                            "max_col": min(max_col, MAX_COLUMNS),
                        }
                    )
            element.clear()
    return {"column_widths": widths, "merges": merges, "frozen_rows": frozen_rows}


def _xlsx_formatted(value: Any, number_format: str) -> str:
    """Apply common Excel display formats without evaluating a workbook formula."""
    if value is None:
        return ""
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"
    code = number_format[:MAX_CELL_CHARS]
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        # Excel uses m for both months and minutes; a neighbouring h or s denotes time.
        tokens = re.findall(r'"[^"\n]*"|\\.|AM/PM|[ymdhs]+|.', code, flags=re.IGNORECASE)
        result: list[str] = []
        has_ampm = "am/pm" in code.lower()
        for index, token in enumerate(tokens):
            lower = token.lower()
            if token.startswith('"'):
                result.append(token[1:-1])
            elif token.startswith("\\"):
                result.append(token[1:])
            elif lower == "am/pm":
                result.append("AM" if getattr(value, "hour", 0) < 12 else "PM")
            elif lower.startswith("y"):
                year = getattr(value, "year", 1900)
                result.append(str(year) if len(token) > 2 else f"{year % 100:02d}")
            elif lower.startswith("m"):
                before = "".join(tokens[:index]).lower()
                after = "".join(tokens[index + 1 :]).lower()
                minute = "h" in before.rsplit(" ", 1)[-1] or after.startswith(":s")
                month = getattr(value, "month", 1)
                component = getattr(value, "minute", 0) if minute else month
                if not minute and len(token) >= 3:
                    result.append(datetime.date(2000, month, 1).strftime("%B" if len(token) >= 4 else "%b"))
                else:
                    result.append(str(component).zfill(2) if len(token) == 2 else str(component))
            elif lower.startswith("d"):
                day = getattr(value, "day", 1)
                if len(token) >= 3 and isinstance(value, datetime.date):
                    result.append(value.strftime("%A" if len(token) >= 4 else "%a"))
                else:
                    result.append(str(day).zfill(2) if len(token) == 2 else str(day))
            elif lower.startswith("h"):
                hour = getattr(value, "hour", 0)
                hour = (hour % 12 or 12) if has_ampm else hour
                result.append(str(hour).zfill(2) if len(token) == 2 else str(hour))
            elif lower.startswith("s"):
                second = getattr(value, "second", 0)
                result.append(str(second).zfill(2) if len(token) == 2 else str(second))
            else:
                result.append(token)
        return (
            "".join(result)
            if any(t.lower().startswith(("y", "m", "d", "h", "s")) for t in tokens)
            else value.isoformat()
        )
    if not isinstance(value, (int, float, Decimal)) or code.lower() in {"general", "@", ""}:
        return str(value)
    sections = code.split(";")
    section = (
        sections[1]
        if value < 0 and len(sections) > 1
        else sections[2]
        if value == 0 and len(sections) > 2
        else sections[0]
    )
    section = re.sub(r"\[(?!\$)[^]]*\]", "", section)
    section = re.sub(r"\[\$([^]-]*)[^]]*\]", r"\1", section)
    section = re.sub(r"_.|\*.", "", section)
    section = re.sub(r'"([^"\n]*)"|\\(.)', lambda match: match[1] if match[1] is not None else match[2], section)
    match = re.search(r"[0#?][0#?,]*(?:\.[0#?]+)?(?:[Ee][+-]0+)?", section)
    if not match:
        return section
    pattern = match[0]
    number = abs(value) if value < 0 and len(sections) > 1 else value
    number *= 100 ** section.count("%")
    decimals = min(20, len(pattern.split(".", 1)[1].split("E")[0].split("e")[0])) if "." in pattern else 0
    if "e" in pattern.lower():
        rendered = f"{number:.{decimals}E}"
    else:
        rendered = format(number, ("," if "," in pattern else "") + "." + str(decimals) + "f")
        if "." not in pattern and "," not in pattern:
            sign = "-" if rendered.startswith("-") else ""
            rendered = sign + rendered.lstrip("-").zfill(pattern.count("0"))
    return section[: match.start()] + rendered + section[match.end() :]


def _xlsx_detail(cell: Any, cached: Any, warnings: list[str]) -> tuple[Any, dict[str, Any]]:
    """Keep formula source separate from its cached, inert display value."""
    formula = None
    raw = getattr(cell, "value", None)
    if getattr(cell, "data_type", None) == "f":
        formula = _truncate_text(str(getattr(raw, "text", raw)), warnings)
        value = getattr(cached, "value", None)
        if value is None:
            _warn_once(warnings, MISSING_FORMULA_CACHE_WARNING)
    else:
        value = raw
    number_format = _truncate_text(getattr(cell, "number_format", None) or "General", warnings)
    return _xlsx_cell(value, warnings), {
        "formatted": _truncate_text(_xlsx_formatted(value, number_format), warnings),
        "formula": formula,
        "number_format": number_format,
    }


def _recalculated_xlsx_values(
    path: Path, sheet: str, missing: list[tuple[int, int]], warnings: list[str]
) -> dict[tuple[int, int], Any]:
    """Obtain only missing requested values from a bounded private native calculation."""
    try:
        with tempfile.TemporaryDirectory(prefix="branchyard-formula-cache-") as temporary:
            calculated = Path(temporary) / "calculated.xlsx"
            recalculate_workbook(
                path, calculated, max_output_bytes=MAX_INPUT_BYTES, timeout=min(20, PROCESS_TIMEOUT_SECONDS)
            )
            payload = _read_regular(calculated, MAX_INPUT_BYTES)
            _check_office_archive(payload, "xlsx")
            workbook = library("openpyxl").load_workbook(
                io.BytesIO(payload), read_only=True, data_only=True, keep_links=False
            )
            try:
                if sheet not in workbook.sheetnames:
                    raise ArtifactError("Calculated workbook did not preserve the selected worksheet")
                worksheet = workbook[sheet]
                worksheet.reset_dimensions()
                coordinates = set(missing)
                first = min(row for row, _ in missing)
                last = max(row for row, _ in missing)
                max_col = max(column for _, column in missing)
                values: dict[tuple[int, int], Any] = {}
                for row_number, row in enumerate(
                    worksheet.iter_rows(min_row=first, max_row=last, max_col=max_col, values_only=True), first
                ):
                    for column, value in enumerate(row, 1):
                        if (row_number, column) in coordinates and value is not None:
                            _xlsx_cell(value, warnings)  # Validate native cache values before publishing any of them.
                            values[(row_number, column)] = value
                return values
            finally:
                workbook.close()
    except Exception as error:
        _warn_once(warnings, "Formula values could not be recalculated: " + _truncate_text(str(error), warnings))
        return {}


def _preview_xlsx_table(path: Path, sheet: str | None, offset: int, limit: int) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    _check_office_archive(payload, "xlsx")
    openpyxl = library("openpyxl")
    column_letter = library("openpyxl.utils.cell").get_column_letter
    warnings: list[str] = []
    workbook = cached_workbook = None
    try:
        workbook = openpyxl.load_workbook(io.BytesIO(payload), read_only=True, data_only=False, keep_links=False)
        cached_workbook = openpyxl.load_workbook(io.BytesIO(payload), read_only=True, data_only=True, keep_links=False)
        sheets = list(workbook.sheetnames)
        if not sheets:
            raise ArtifactError("Workbook contains no worksheets")
        selected = sheets[0] if sheet is None else sheet
        if selected not in sheets:
            raise ArtifactError("Worksheet does not exist: " + str(selected))
        worksheet, cached_worksheet = workbook[selected], cached_workbook[selected]
        layout = _xlsx_layout(workbook, worksheet, offset, limit)
        # Ignore the stored dimension: styled empty cells can inflate it and careless writers understate it.
        worksheet.reset_dimensions()
        cached_worksheet.reset_dimensions()
        row_iter = zip_longest(worksheet.iter_rows(), cached_worksheet.iter_rows(), fillvalue=())
        columns: list[str] = []
        header_details: list[dict[str, Any]] = []
        rows: list[list[Any]] = []
        cell_details: list[list[dict[str, Any]]] = []
        next_offset = None
        missing_formulas: list[tuple[int, int]] = []
        for row_number, (raw_row, cached_row) in enumerate(row_iter):
            if row_number > MAX_SCAN_ROWS:
                _warn_once(warnings, "Row preview stops at " + str(MAX_SCAN_ROWS) + " rows")
                break
            raw_values = [getattr(cell, "value", None) for cell in raw_row]
            if any(value is not None for value in raw_values[MAX_COLUMNS:]):
                _warn_once(warnings, "Truncated columns to " + str(MAX_COLUMNS))
            raw_values = raw_values[:MAX_COLUMNS]
            while raw_values and raw_values[-1] is None:
                raw_values.pop()
            width = min(MAX_COLUMNS, len(raw_values))
            if row_number == 0 or offset < row_number <= offset + limit:
                converted = [
                    _xlsx_detail(raw_row[index], cached_row[index] if index < len(cached_row) else None, warnings)
                    for index in range(width)
                ]
                missing_formulas.extend(
                    (row_number + 1, index + 1)
                    for index, (value, detail) in enumerate(converted)
                    if value is None and detail["formula"] is not None
                )
                if row_number == 0:
                    columns = [
                        detail["formatted"] or str(column_letter(index + 1))
                        for index, (_, detail) in enumerate(converted)
                    ]
                    header_details = [detail for _, detail in converted]
                else:
                    rows.append([value for value, _ in converted])
                    cell_details.append([detail for _, detail in converted])
            while len(columns) < width:
                columns.append(str(column_letter(len(columns) + 1)))
            if row_number > offset + limit:
                next_offset = offset + len(rows) if offset + len(rows) < MAX_SCAN_ROWS else None
                break
        if missing_formulas:
            calculated_values = _recalculated_xlsx_values(path, selected, missing_formulas, warnings)
            for (row_number, column), value in calculated_values.items():
                if row_number == 1:
                    detail = header_details[column - 1]
                else:
                    index = row_number - offset - 2
                    rows[index][column - 1] = _xlsx_cell(value, warnings)
                    detail = cell_details[index][column - 1]
                detail["formatted"] = _truncate_text(_xlsx_formatted(value, detail["number_format"]), warnings)
                if row_number == 1:
                    columns[column - 1] = detail["formatted"] or str(column_letter(column))
            if len(calculated_values) == len(missing_formulas):
                warnings.remove(MISSING_FORMULA_CACHE_WARNING)
        merged_width = max((item["max_col"] for item in layout["merges"]), default=0)
        while len(columns) < merged_width:
            columns.append(str(column_letter(len(columns) + 1)))
        empty_detail = {"formatted": "", "formula": None, "number_format": "General"}
        for row, details in zip(rows, cell_details):
            row.extend([None] * (len(columns) - len(row)))
            details.extend([empty_detail.copy() for _ in range(len(columns) - len(details))])
        header_details.extend([empty_detail.copy() for _ in range(len(columns) - len(header_details))])
        layout["column_widths"] = layout["column_widths"][: len(columns)]
    except Exception as error:
        if isinstance(error, (ArtifactError, Unavailable)):
            raise
        raise ArtifactError("Unable to open XLSX workbook: " + str(error)) from error
    finally:
        if cached_workbook is not None:
            cached_workbook.close()
        if workbook is not None:
            workbook.close()
    return {
        "status": "passed",
        "kind": "table",
        "format": "xlsx",
        "columns": columns,
        "rows": rows,
        "sheets": sheets,
        "sheet": selected,
        "offset": offset,
        "next_offset": next_offset,
        "warnings": warnings,
        "header_details": header_details,
        "cell_details": cell_details,
        **layout,
    }


def _preview_document_page(path: Path, out: Path, fmt: str, page: int, sheet: str | None = None) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if fmt in REQUIRED_OFFICE_PARTS:
        _check_office_archive(payload, fmt)
    elif fmt == "pdf" and not payload.lstrip().startswith(b"%PDF-"):
        raise ArtifactError("Malformed PDF input: missing %PDF- header")
    image_name = f"page-{page}.png"
    target = out / image_name
    if Path(os.path.abspath(target)) == Path(os.path.abspath(path)):
        raise ArtifactError("Preview output image must not overwrite the source file")
    rendered = render_page(
        path,
        target,
        page=page,
        max_pages=MAX_PAGES,
        max_dimension=MAX_PREVIEW_DIMENSION,
        max_output_bytes=MAX_OUTPUT_IMAGE_BYTES,
        timeout=PROCESS_TIMEOUT_SECONDS,
        **({"sheet": sheet} if sheet is not None else {}),
    )
    return {
        "status": "passed",
        "kind": "pages",
        "format": fmt,
        "page": page,
        "pages": rendered["pages"],
        "image": image_name,
        "width": rendered["width"],
        "height": rendered["height"],
        "warnings": rendered["warnings"],
    }


def _preview_image(path: Path, out: Path, fmt: str, page: int) -> dict[str, Any]:
    if page != 1:
        raise ArtifactError("Image preview has 1 page; requested page " + str(page) + " is out of range")
    payload = _read_regular(path, MAX_INPUT_BYTES)
    stripped = payload[:256].lstrip().lower()
    if stripped.startswith((b"<svg", b"<!doctype html", b"<html", b"<?xml")):
        raise ArtifactError("Active markup disguised as a raster image is refused")
    pil_image = library("PIL.Image")
    warnings: list[str] = []
    image_name = "page-1.png"
    target = out / image_name
    if Path(os.path.abspath(target)) == Path(os.path.abspath(path)):
        raise ArtifactError("Preview output image must not overwrite the source file")
    try:
        with pil_image.open(io.BytesIO(payload)) as probe:
            if (
                probe.width <= 0
                or probe.height <= 0
                or probe.width > MAX_IMAGE_DIMENSION
                or probe.height > MAX_IMAGE_DIMENSION
                or probe.width * probe.height > MAX_IMAGE_PIXELS
            ):
                raise ArtifactError("Image dimensions exceed preview bounds")
            probe.verify()
        with pil_image.open(io.BytesIO(payload)) as decoded:
            decoded.load()
            if getattr(decoded, "is_animated", False) and getattr(decoded, "n_frames", 1) > 1:
                decoded.seek(0)
                _warn_once(warnings, "Animated image preview renders frame 1 only")
            mode = "RGBA" if "A" in decoded.getbands() or "transparency" in decoded.info else "RGB"
            converted = decoded.convert(mode)
            if converted.width > MAX_PREVIEW_DIMENSION or converted.height > MAX_PREVIEW_DIMENSION:
                converted.thumbnail((MAX_PREVIEW_DIMENSION, MAX_PREVIEW_DIMENSION))
                _warn_once(warnings, "Scaled image within " + str(MAX_PREVIEW_DIMENSION) + "px dimension bound")
            width, height = converted.width, converted.height
            buffer = io.BytesIO()
            converted.save(buffer, format="PNG")
            png_bytes = buffer.getvalue()
        with pil_image.open(io.BytesIO(png_bytes)) as check_img:
            check_img.verify()
    except Exception as error:
        if isinstance(error, ArtifactError):
            raise
        raise ArtifactError("Invalid or unsupported raster image: " + str(error)) from error
    if not png_bytes or len(png_bytes) > MAX_OUTPUT_IMAGE_BYTES:
        raise ArtifactError("Converted PNG exceeds the output byte limit")
    out.mkdir(parents=True, exist_ok=True)
    with output_file(target) as staged:
        staged.write_bytes(png_bytes)
    return {
        "status": "passed",
        "kind": "pages",
        "format": fmt,
        "page": 1,
        "pages": 1,
        "image": image_name,
        "width": width,
        "height": height,
        "warnings": warnings,
    }


def _preview_text(path: Path, fmt: str) -> dict[str, Any]:
    payload = _read_regular(path, MAX_INPUT_BYTES)
    if b"\x00" in payload:
        raise ArtifactError("Text preview input contains NUL bytes")
    warnings: list[str] = []
    truncated = False
    raw_slice = payload
    if len(payload) > MAX_TEXT_BYTES:
        raw_slice = payload[:MAX_TEXT_BYTES]
        truncated = True
        _warn_once(warnings, "Truncated text preview to " + str(MAX_TEXT_BYTES) + " bytes")
    try:
        text = raw_slice.decode("utf-8-sig") if not truncated else raw_slice.decode("utf-8-sig", errors="ignore")
        if truncated:
            payload[: min(len(payload), 4096)].decode("utf-8-sig")
    except UnicodeDecodeError as error:
        raise ArtifactError("Text preview input must be valid UTF-8") from error
    if fmt == "json" and not truncated:
        try:
            json.loads(text)
        except ValueError as error:
            raise ArtifactError("Malformed JSON input: " + str(error)) from error
    if len(text) > MAX_TEXT_CHARS:
        text = text[:MAX_TEXT_CHARS]
        truncated = True
        _warn_once(warnings, "Truncated text preview to " + str(MAX_TEXT_CHARS) + " characters")
    return {
        "status": "passed",
        "kind": "text",
        "format": fmt,
        "text": text,
        "truncated": truncated,
        "warnings": warnings,
    }


def preview(
    path: Path,
    out: Path,
    *,
    view: str = "auto",
    page: int = 1,
    sheet: str | None = None,
    offset: int = 0,
    limit: int = 100,
) -> dict[str, Any]:
    """Generate a bounded table, page or text preview without modifying the source file."""
    source, out_dir = _validate_paths(path, out)
    if view not in {"auto", "table", "pages", "text"}:
        raise ArtifactError("view must be auto, table or pages")
    if isinstance(page, bool) or not isinstance(page, int) or page < 1:
        raise ArtifactError("page must be a positive 1-based integer")
    if isinstance(offset, bool) or not isinstance(offset, int) or offset < 0 or offset >= MAX_SCAN_ROWS:
        raise ArtifactError("offset must be a non-negative integer within the row scan limit")
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= MAX_LIMIT:
        raise ArtifactError("limit must be an integer between 1 and " + str(MAX_LIMIT))
    if sheet is not None and (not isinstance(sheet, str) or not sheet.strip()):
        raise ArtifactError("sheet must be a nonempty worksheet name")

    suffix = source.suffix.lower()
    if suffix in MACRO_SUFFIXES:
        raise ArtifactError("Macro-enabled Office formats are refused for preview: " + suffix)
    if suffix in ACTIVE_MARKUP_SUFFIXES:
        raise ArtifactError("Active or executable markup formats (SVG/HTML/XML) are refused: " + suffix)

    if suffix == ".csv":
        if view not in {"auto", "table"}:
            raise ArtifactError("CSV files support table preview only")
        if sheet is not None:
            raise ArtifactError("CSV preview does not support --sheet")
        if page != 1:
            raise ArtifactError("Table preview uses --offset and --limit rather than --page")
        return _preview_csv(source, offset, limit)

    if suffix == ".xlsx":
        resolved_view = "table" if view == "auto" else view
        if resolved_view == "table":
            if page != 1:
                raise ArtifactError("Table preview uses --offset and --limit rather than --page")
            return _preview_xlsx_table(source, sheet, offset, limit)
        if resolved_view == "pages":
            if offset != 0:
                raise ArtifactError("Page preview uses --page rather than --offset")
            return _preview_document_page(source, out_dir, "xlsx", page, sheet)
        raise ArtifactError("XLSX files support table or pages preview")

    if suffix in {".docx", ".pptx", ".pdf"}:
        fmt = suffix.lstrip(".")
        if view not in {"auto", "pages"}:
            raise ArtifactError(fmt.upper() + " files support pages preview only")
        if sheet is not None:
            raise ArtifactError(fmt.upper() + " preview does not support --sheet")
        if offset != 0:
            raise ArtifactError("Page preview uses --page rather than --offset")
        return _preview_document_page(source, out_dir, fmt, page)

    if suffix in IMAGE_FORMATS:
        fmt = IMAGE_FORMATS[suffix]
        if view not in {"auto", "pages"}:
            raise ArtifactError("Image files support pages preview only")
        if sheet is not None:
            raise ArtifactError("Image preview does not support --sheet")
        if offset != 0:
            raise ArtifactError("Image preview does not support --offset")
        return _preview_image(source, out_dir, fmt, page)

    if suffix in TEXT_FORMATS:
        fmt = TEXT_FORMATS[suffix]
        if view not in {"auto", "text"}:
            raise ArtifactError(fmt.upper() + " files support text preview only")
        if sheet is not None or page != 1 or offset != 0:
            raise ArtifactError("Text preview does not support --page, --sheet or --offset")
        return _preview_text(source, fmt)

    raise ArtifactError("Unsupported preview file format: " + (suffix or source.name))
