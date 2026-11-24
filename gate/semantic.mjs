// What a workbook file says, read without the engine that wrote it.
//
// The gate compares files byte for byte, which proves the browser build writes what the native
// one writes and nothing about whether either writes the right thing. This reads an .xlsx with a
// plain zip and XML reader and gives its meaning as data: per sheet the cells (value, formula,
// resolved style, formatting inside the text), merges and validations; and the workbook's names.
// Two files with the same meaning give the same data, whatever the order of their style tables.
// The expected data of a fixed script is committed (semantic-golden.json), so a build whose
// engine starts writing something else fails here even when browser and native still agree.
import { unzipSync } from "fflate";
import { XMLParser } from "fast-xml-parser";

const LISTS = new Set(["sheet", "definedName", "si", "r", "t", "font", "fill", "border", "xf", "numFmt", "row", "c", "mergeCell", "dataValidation", "Relationship"]);
const parser = new XMLParser({
  ignoreAttributes: false,
  attributeNamePrefix: "@",
  textNodeName: "#",
  parseTagValue: false,
  parseAttributeValue: false,
  trimValues: false,
  removeNSPrefix: true,
  isArray: (name) => LISTS.has(name),
  processEntities: true,
});
const text = (bytes) => new TextDecoder().decode(bytes);
const str = (node) => (node == null ? "" : typeof node === "string" ? node : String(node["#"] ?? ""));
const on = (node) => node !== undefined && node?.["@val"] !== "0" && node?.["@val"] !== "false";
const colour = (node) => (node?.["@rgb"] ? String(node["@rgb"]).slice(-6).toLowerCase() : node?.["@theme"] !== undefined ? `theme${node["@theme"]}` : undefined);
const clean = (o) => {
  const out = {};
  for (const [k, v] of Object.entries(o)) if (v !== undefined && v !== false && v !== "" && !(typeof v === "object" && v !== null && !Array.isArray(v) && Object.keys(v).length === 0)) out[k] = v;
  return out;
};

function fontOf(f) {
  if (!f) return {};
  return clean({ bold: on(f.b), italic: on(f.i), underline: f.u !== undefined && f.u?.["@val"] !== "none", strike: on(f.strike), color: colour(f.color) });
}

/** One formatted stretch of a shared string: where it is (UTF-16 units) and how it looks. */
function richOf(si) {
  if (!si.r) return { text: str(si.t?.[0]), runs: [] };
  let at = 0;
  let all = "";
  const runs = [];
  for (const r of si.r) {
    const part = (r.t ?? []).map(str).join("");
    const look = fontOf(r.rPr);
    if (Object.keys(look).length > 0 && part.length > 0) runs.push({ start: at, end: at + part.length, ...look });
    at += part.length;
    all += part;
  }
  return { text: all, runs };
}

function stylesOf(xml) {
  const sheet = xml ? parser.parse(xml).styleSheet ?? {} : {};
  const formats = new Map((sheet.numFmts?.numFmt ?? []).map((n) => [String(n["@numFmtId"]), String(n["@formatCode"])]));
  const fonts = sheet.fonts?.font ?? [];
  const fills = sheet.fills?.fill ?? [];
  const borders = sheet.borders?.border ?? [];
  const all = (sheet.cellXfs?.xf ?? []).map((xf) => {
    const id = String(xf["@numFmtId"] ?? "0");
    const fill = fills[Number(xf["@fillId"] ?? 0)]?.patternFill;
    const border = borders[Number(xf["@borderId"] ?? 0)] ?? {};
    const edges = clean(Object.fromEntries(["left", "right", "top", "bottom"].map((side) => [side, border[side]?.["@style"]])));
    const a = xf.alignment;
    return clean({
      // A built-in format has no code in the file: its number is its name.
      numFmt: formats.get(id) ?? (id === "0" ? undefined : `#${id}`),
      ...fontOf(fonts[Number(xf["@fontId"] ?? 0)]),
      fill: fill?.["@patternType"] === "solid" ? colour(fill.fgColor) : undefined,
      borders: edges,
      align: a ? clean({ h: a["@horizontal"], v: a["@vertical"], wrap: a["@wrapText"] === "1" || a["@wrapText"] === "true" }) : undefined,
    });
  });
  // Said once, not on every cell: what the default cell format (the first one) already has.
  const base = all[0] ?? {};
  return all.map((style) => clean(Object.fromEntries(Object.entries(style).map(([k, v]) => [k, JSON.stringify(v) === JSON.stringify(base[k]) ? undefined : v]))));
}

/** The meaning of the workbook in `bytes`, as plain data with every list in a fixed order. */
export function readWorkbook(bytes) {
  const zip = unzipSync(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
  const part = (name) => (zip[name] ? text(zip[name]) : null);
  const book = parser.parse(part("xl/workbook.xml") ?? "<workbook/>").workbook ?? {};
  const rels = new Map(((parser.parse(part("xl/_rels/workbook.xml.rels") ?? "<Relationships/>").Relationships ?? {}).Relationship ?? []).map((r) => [r["@Id"], String(r["@Target"]).replace(/^\/?(xl\/)?/, "xl/")]));
  const strings = ((parser.parse(part("xl/sharedStrings.xml") ?? "<sst/>").sst ?? {}).si ?? []).map(richOf);
  const styles = stylesOf(part("xl/styles.xml"));

  const sheets = (book.sheets?.sheet ?? []).map((s) => {
    const xml = part(rels.get(s["@id"]) ?? "") ?? "<worksheet/>";
    const ws = parser.parse(xml).worksheet ?? {};
    const cells = {};
    for (const row of ws.sheetData?.row ?? []) {
      for (const c of row.c ?? []) {
        const kind = c["@t"] ?? "n";
        const raw = c.v === undefined ? undefined : str(c.v);
        let value;
        let rich;
        if (kind === "s") {
          const shared = strings[Number(raw)] ?? { text: "", runs: [] };
          value = shared.text;
          if (shared.runs.length > 0) rich = shared.runs;
        } else if (kind === "inlineStr") {
          const inline = richOf(c.is ?? {});
          value = inline.text;
          if (inline.runs.length > 0) rich = inline.runs;
        } else if (kind === "b") value = raw === "1";
        else if (kind === "n") value = raw === undefined ? undefined : Number(raw);
        else value = raw;
        const style = styles[Number(c["@s"] ?? 0)] ?? {};
        const cell = clean({ type: value === undefined ? undefined : kind === "s" || kind === "inlineStr" ? "str" : kind, value, formula: c.f === undefined ? undefined : str(c.f), rich, style });
        if (Object.keys(cell).length > 0) cells[c["@r"]] = cell;
      }
    }
    return clean({
      name: String(s["@name"]),
      hidden: s["@state"] && s["@state"] !== "visible" ? String(s["@state"]) : undefined,
      cells: Object.fromEntries(Object.entries(cells).sort(([a], [b]) => a.localeCompare(b, "en", { numeric: true }))),
      merges: (ws.mergeCells?.mergeCell ?? []).map((m) => String(m["@ref"])).sort(),
      validations: (ws.dataValidations?.dataValidation ?? [])
        .map((v) => clean({ range: String(v["@sqref"]), type: v["@type"], operator: v["@operator"], formula1: v.formula1 === undefined ? undefined : str(v.formula1), formula2: v.formula2 === undefined ? undefined : str(v.formula2) }))
        .sort((a, b) => a.range.localeCompare(b.range)),
    });
  });
  const names = (book.definedNames?.definedName ?? [])
    .map((n) => clean({ name: String(n["@name"]), value: str(n), hidden: n["@hidden"] === "1" || n["@hidden"] === "true", sheet: n["@localSheetId"] }))
    .sort((a, b) => a.name.localeCompare(b.name));
  return { sheets, names };
}

/** The first places two readings differ, as `path: got … want …` lines (at most `limit`). */
export function differences(got, want, limit = 12) {
  const out = [];
  const walk = (a, b, path) => {
    if (out.length >= limit) return;
    if (a === b) return;
    const objects = a && b && typeof a === "object" && typeof b === "object" && Array.isArray(a) === Array.isArray(b);
    if (!objects) {
      // Numbers a hair apart are the same number written twice.
      if (typeof a === "number" && typeof b === "number" && Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(a), Math.abs(b))) return;
      out.push(`${path || "workbook"}: got ${JSON.stringify(a)?.slice(0, 80)} want ${JSON.stringify(b)?.slice(0, 80)}`);
      return;
    }
    for (const k of new Set([...Object.keys(a), ...Object.keys(b)])) walk(a[k], b[k], path ? `${path}.${k}` : k);
  };
  walk(got, want, "");
  return out;
}
