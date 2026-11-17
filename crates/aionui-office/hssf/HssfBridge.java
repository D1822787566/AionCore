import com.google.gson.*;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.time.*;
import java.time.format.DateTimeFormatter;
import java.time.format.ResolverStyle;
import java.util.*;
import org.apache.poi.hssf.record.SupBookRecord;
import org.apache.poi.hssf.model.HSSFFormulaParser;
import org.apache.poi.hssf.usermodel.*;
import org.apache.poi.poifs.filesystem.*;
import org.apache.poi.ss.usermodel.*;
import org.apache.poi.ss.util.*;

/** One request per process. The Rust coordinator owns paths and the file lifecycle. */
public final class HssfBridge {
    private static final Gson JSON = new GsonBuilder().serializeNulls().create();
    private static final int MAX_CELLS = 10000;

    public static void main(String[] args) {
        System.setOut(new PrintStream(new FileOutputStream(FileDescriptor.out), true, StandardCharsets.UTF_8));
        try {
            JsonObject input = JsonParser.parseReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8)).getAsJsonObject();
            JsonObject result = execute(input);
            System.out.println(JSON.toJson(Map.of("success", true, "data", result)));
        } catch (Exception error) {
            String message = error.getMessage();
            System.out.println(JSON.toJson(Map.of("success", false,
                "error", Map.of("code", "hssf_operation_failed", "message",
                    message == null ? error.getClass().getSimpleName() : message))));
            System.exit(2);
        }
    }

    private static JsonObject execute(JsonObject input) throws Exception {
        Path path = Path.of(text(input, "path"));
        try (InputStream stream = Files.newInputStream(path);
             HSSFWorkbook workbook = new HSSFWorkbook(stream, true)) {
            String action = text(input, "action");
            JsonObject result = metadata(workbook);
            switch (action) {
                case "inspect":
                    if (input.has("sheet")) {
                        result.add("cells", read(workbook, text(input, "sheet"), text(input, "range")));
                    }
                    return result;
                case "verify":
                    verify(workbook, input.getAsJsonArray("changes"));
                    result.addProperty("readback", "passed");
                    return result;
                case "write":
                    write(workbook, input, result);
                    return result;
                default:
                    throw new IllegalArgumentException("unknown HSSF action");
            }
        }
    }

    private static JsonObject metadata(HSSFWorkbook workbook) throws IOException {
        JsonObject result = new JsonObject();
        result.addProperty("format", "xls");
        result.addProperty("date_system", workbook.getInternalWorkbook().isUsing1904DateWindowing() ? "1904" : "1900");
        result.addProperty("protected", workbook.isWriteProtected());
        JsonArray sheets = new JsonArray();
        Set<String> complex = new TreeSet<>();
        DirectoryNode root = workbook.getDirectory();
        if (root != null) {
            for (Entry entry : root) {
                String name = entry.getName();
                if (!Set.of("Workbook", "Book", "\u0005SummaryInformation", "\u0005DocumentSummaryInformation").contains(name)) {
                    complex.add("compound_entry:" + name);
                }
            }
        }
        for (org.apache.poi.hssf.record.Record record : workbook.getInternalWorkbook().getRecords()) {
            if (record instanceof SupBookRecord links && links.isExternalReferences()) complex.add("external_links");
            String name = record.getClass().getSimpleName();
            if (name.startsWith("Pivot") || name.startsWith("SX") || name.startsWith("Sx")) complex.add("pivot_records");
        }
        if (!workbook.getAllEmbeddedObjects().isEmpty()) complex.add("embedded_objects");
        for (int i = 0; i < workbook.getNumberOfSheets(); i++) {
            HSSFSheet sheet = workbook.getSheetAt(i);
            JsonObject descriptor = new JsonObject();
            descriptor.addProperty("name", sheet.getSheetName());
            descriptor.addProperty("last_row", sheet.getPhysicalNumberOfRows() == 0 ? 0 : sheet.getLastRowNum() + 1);
            descriptor.addProperty("protected", sheet.getProtect());
            descriptor.addProperty("visibility", workbook.getSheetVisibility(i).name().toLowerCase(Locale.ROOT));
            JsonArray merges = new JsonArray();
            for (CellRangeAddress range : sheet.getMergedRegions()) merges.add(range.formatAsString());
            descriptor.add("merged_ranges", merges);
            sheets.add(descriptor);
            for (org.apache.poi.hssf.record.RecordBase record : sheet.getSheet().getRecords()) {
                String name = record.getClass().getSimpleName();
                if (name.startsWith("Chart") || name.startsWith("Pivot") || name.startsWith("SX") || name.startsWith("Sx")) {
                    complex.add("chart_or_pivot_records");
                }
            }
            HSSFPatriarch drawing = sheet.getDrawingPatriarch();
            if (drawing != null) {
                for (HSSFShape shape : drawing.getChildren()) {
                    // Comments are covered by the preservation tests; other drawings are not.
                    if (!(shape instanceof HSSFComment)) complex.add("drawing_objects");
                }
            }
        }
        result.add("sheets", sheets);
        result.add("complex_objects", JSON.toJsonTree(complex));
        result.addProperty("complex_check", "known_biff_and_compound_objects");
        result.addProperty("editable", complex.isEmpty() && !workbook.isWriteProtected());
        return result;
    }

    private static JsonArray read(HSSFWorkbook workbook, String name, String address) {
        HSSFSheet sheet = sheet(workbook, name);
        CellRangeAddress range = CellRangeAddress.valueOf(address);
        range.validate(org.apache.poi.ss.SpreadsheetVersion.EXCEL97);
        long count = (long) (range.getLastRow() - range.getFirstRow() + 1)
            * (range.getLastColumn() - range.getFirstColumn() + 1);
        require(count <= MAX_CELLS, "range exceeds 10000 cells; request smaller ranges");
        DataFormatter formatter = new DataFormatter(Locale.ROOT);
        formatter.setUseCachedValuesForFormulaCells(true);
        JsonArray cells = new JsonArray();
        for (int r = range.getFirstRow(); r <= range.getLastRow(); r++) {
            for (int c = range.getFirstColumn(); c <= range.getLastColumn(); c++) {
                HSSFRow row = sheet.getRow(r);
                HSSFCell cell = row == null ? null : row.getCell(c);
                JsonObject value = new JsonObject();
                value.addProperty("cell", new CellReference(r, c).formatAsString());
                value.addProperty("type", cell == null ? "blank" : cell.getCellType().name().toLowerCase(Locale.ROOT));
                value.add("value", cell == null ? JsonNull.INSTANCE : raw(cell, cell.getCellType()));
                value.addProperty("display", cell == null ? "" : formatter.formatCellValue(cell));
                value.addProperty("number_format", cell == null ? "General" : cell.getCellStyle().getDataFormatString());
                if (cell != null && cell.getCellType() == CellType.FORMULA) {
                    value.addProperty("formula", cell.getCellFormula());
                    value.addProperty("cached_type", cell.getCachedFormulaResultType().name().toLowerCase(Locale.ROOT));
                    value.add("cached_value", raw(cell, cell.getCachedFormulaResultType()));
                }
                cells.add(value);
            }
        }
        return cells;
    }

    private static JsonElement raw(HSSFCell cell, CellType type) {
        return switch (type) {
            case STRING -> new JsonPrimitive(cell.getStringCellValue());
            case NUMERIC -> new JsonPrimitive(cell.getNumericCellValue());
            case BOOLEAN -> new JsonPrimitive(cell.getBooleanCellValue());
            case FORMULA -> new JsonPrimitive(cell.getCellFormula());
            case ERROR -> new JsonPrimitive(FormulaError.forInt(cell.getErrorCellValue()).getString());
            default -> JsonNull.INSTANCE;
        };
    }

    private static void write(HSSFWorkbook workbook, JsonObject input, JsonObject result) throws Exception {
        require(result.get("editable").getAsBoolean(), "workbook contains protection or unaccepted complex objects");
        JsonArray changes = input.getAsJsonArray("changes");
        require(changes != null && !changes.isEmpty() && changes.size() <= MAX_CELLS, "changes must contain 1..10000 cells");
        Set<String> targets = new HashSet<>();
        for (JsonElement element : changes) {
            JsonObject change = element.getAsJsonObject();
            HSSFSheet sheet = sheet(workbook, text(change, "sheet"));
            CellReference reference = reference(text(change, "cell"));
            String key = sheet.getSheetName() + "!" + reference.formatAsString();
            require(targets.add(key), "duplicate target: " + key);
            require(!sheet.getProtect(), "protected sheet: " + sheet.getSheetName());
            for (CellRangeAddress merge : sheet.getMergedRegions()) {
                require(!merge.isInRange(reference.getRow(), reference.getCol())
                    || (reference.getRow() == merge.getFirstRow() && reference.getCol() == merge.getFirstColumn()),
                    "merged cell must use anchor: " + key);
            }
            HSSFRow row = sheet.getRow(reference.getRow());
            HSSFCell cell = row == null ? null : row.getCell(reference.getCol());
            require(cell == null || !cell.isPartOfArrayFormulaGroup(), "array formula cannot be partially changed: " + key);
            validateValue(workbook, change, cell);
        }
        Map<String, String> before = snapshot(workbook);
        Map<String, String> oldErrors = formulaErrors(workbook);
        for (JsonElement element : changes) {
            JsonObject change = element.getAsJsonObject();
            HSSFSheet sheet = sheet(workbook, text(change, "sheet"));
            CellReference reference = reference(text(change, "cell"));
            HSSFRow row = sheet.getRow(reference.getRow());
            if (row == null) row = sheet.createRow(reference.getRow());
            HSSFCell cell = row.getCell(reference.getCol());
            if (cell == null) cell = row.createCell(reference.getCol());
            cell.setBlank();
            switch (text(change, "type")) {
                case "string": cell.setCellValue(change.get("value").getAsString()); break;
                case "number": cell.setCellValue(change.get("value").getAsDouble()); break;
                case "boolean": cell.setCellValue(change.get("value").getAsBoolean()); break;
                case "date": cell.setCellValue(DateUtil.getExcelDate(date(text(change, "value")),
                    workbook.getInternalWorkbook().isUsing1904DateWindowing())); break;
                case "formula": cell.setCellFormula(text(change, "value")); break;
                case "blank": break;
                default: throw new IllegalArgumentException("unknown write type");
            }
        }
        JsonArray diagnostics = new JsonArray();
        FormulaEvaluator evaluator = workbook.getCreationHelper().createFormulaEvaluator();
        for (Sheet sheet : workbook) {
            for (Row row : sheet) for (Cell cell : row) {
                if (cell.getCellType() != CellType.FORMULA) continue;
                try {
                    evaluator.evaluateFormulaCell(cell);
                } catch (RuntimeException error) {
                    JsonObject diagnostic = new JsonObject();
                    diagnostic.addProperty("sheet", sheet.getSheetName());
                    diagnostic.addProperty("cell", cell.getAddress().formatAsString());
                    diagnostic.addProperty("reason", error.getClass().getSimpleName());
                    diagnostics.add(diagnostic);
                }
            }
        }
        Map<String, String> errors = formulaErrors(workbook);
        errors.entrySet().removeIf(e -> e.getValue().equals(oldErrors.get(e.getKey())) && !targets.contains(e.getKey()));
        require(errors.isEmpty(), "new or modified formula returned Excel error: " + errors.keySet());
        Map<String, String> after = snapshot(workbook);
        verifyPreserved(before, after, targets);
        Path output = Path.of(text(input, "output"));
        try (OutputStream stream = Files.newOutputStream(output, StandardOpenOption.CREATE_NEW)) {
            workbook.write(stream);
        }
        try (InputStream stream = Files.newInputStream(output); HSSFWorkbook saved = new HSSFWorkbook(stream, true)) {
            verify(saved, changes);
            verifyPreserved(after, snapshot(saved), Set.of());
        }
        result.addProperty("save", "passed");
        result.addProperty("readback", "passed");
        result.addProperty("recalculation", diagnostics.isEmpty() ? "passed" : "failed");
        result.add("diagnostics", diagnostics);
        result.addProperty("changed_cells", changes.size());
    }

    private static void validateValue(HSSFWorkbook workbook, JsonObject change, HSSFCell cell) {
        String type = text(change, "type");
        JsonElement value = change.get("value");
        switch (type) {
            case "blank":
                require(value == null, "blank must not contain value");
                break;
            case "string":
            case "date":
            case "formula":
                require(value != null && value.isJsonPrimitive() && value.getAsJsonPrimitive().isString(), "value must be a string");
                if (type.equals("string")) require(value.getAsString().length() <= 32767, "XLS string exceeds 32767 characters");
                if (type.equals("formula")) require(!value.getAsString().isBlank() && !value.getAsString().startsWith("="), "formula must omit leading =");
                if (type.equals("date")) {
                    double serial = DateUtil.getExcelDate(date(value.getAsString()),
                        workbook.getInternalWorkbook().isUsing1904DateWindowing());
                    require(serial >= 0, "date precedes the workbook date system");
                    require(cell != null && DateUtil.isCellDateFormatted(cell), "date requires an existing date-formatted cell");
                }
                break;
            case "number":
                require(value != null && value.isJsonPrimitive() && value.getAsJsonPrimitive().isNumber()
                    && Double.isFinite(value.getAsDouble()), "value must be a finite number");
                break;
            case "boolean":
                require(value != null && value.isJsonPrimitive() && value.getAsJsonPrimitive().isBoolean(), "value must be boolean");
                break;
            default: throw new IllegalArgumentException("unsupported write type");
        }
    }

    private static LocalDateTime date(String value) {
        if (value.matches("\\d{4}-\\d{2}-\\d{2}")) return LocalDate.parse(value).atStartOfDay();
        require(value.matches("\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}"), "date requires YYYY-MM-DD or YYYY-MM-DDTHH:mm:ss");
        return LocalDateTime.parse(value, DateTimeFormatter.ofPattern("uuuu-MM-dd'T'HH:mm:ss").withResolverStyle(ResolverStyle.STRICT));
    }

    private static void verify(HSSFWorkbook workbook, JsonArray changes) {
        if (changes == null) return;
        for (JsonElement element : changes) {
            JsonObject change = element.getAsJsonObject();
            HSSFSheet sheet = sheet(workbook, text(change, "sheet"));
            CellReference reference = reference(text(change, "cell"));
            HSSFRow row = sheet.getRow(reference.getRow());
            HSSFCell cell = row == null ? null : row.getCell(reference.getCol());
            String type = text(change, "type");
            boolean matches = switch (type) {
                case "blank" -> cell == null || cell.getCellType() == CellType.BLANK;
                case "string" -> cell != null && cell.getCellType() == CellType.STRING && cell.getStringCellValue().equals(text(change, "value"));
                case "formula" -> cell != null && cell.getCellType() == CellType.FORMULA
                    && cell.getCellFormula().equals(HSSFFormulaParser.toFormulaString(workbook,
                        HSSFFormulaParser.parse(text(change, "value"), workbook,
                            org.apache.poi.ss.formula.FormulaType.CELL, workbook.getSheetIndex(sheet))));
                case "boolean" -> cell != null && cell.getCellType() == CellType.BOOLEAN && cell.getBooleanCellValue() == change.get("value").getAsBoolean();
                case "number" -> cell != null && cell.getCellType() == CellType.NUMERIC && Double.compare(cell.getNumericCellValue(), change.get("value").getAsDouble()) == 0;
                case "date" -> cell != null && cell.getCellType() == CellType.NUMERIC && Math.abs(cell.getNumericCellValue() - DateUtil.getExcelDate(date(text(change, "value")), workbook.getInternalWorkbook().isUsing1904DateWindowing())) < 1e-9;
                default -> false;
            };
            require(matches, "readback mismatch: " + text(change, "sheet") + "!" + text(change, "cell"));
        }
    }

    private static Map<String, String> formulaErrors(HSSFWorkbook workbook) {
        Map<String, String> result = new TreeMap<>();
        for (Sheet sheet : workbook) for (Row row : sheet) for (Cell cell : row) {
            if (cell.getCellType() == CellType.FORMULA && cell.getCachedFormulaResultType() == CellType.ERROR)
                result.put(sheet.getSheetName() + "!" + cell.getAddress().formatAsString(), Byte.toString(cell.getErrorCellValue()));
        }
        return result;
    }

    private static Map<String, String> snapshot(HSSFWorkbook workbook) {
        Map<String, String> result = new TreeMap<>();
        result.put("sheet_count", Integer.toString(workbook.getNumberOfSheets()));
        result.put("style_count", Integer.toString(workbook.getNumCellStyles()));
        for (Name name : workbook.getAllNames()) {
            result.put("name:" + name.getSheetIndex() + ":" + name.getNameName(),
                name.isFunctionName() ? "function" : name.getRefersToFormula());
        }
        for (int i = 0; i < workbook.getNumberOfSheets(); i++) {
            HSSFSheet sheet = workbook.getSheetAt(i);
            String prefix = "sheet:" + i + ":";
            result.put(prefix + "name", sheet.getSheetName());
            result.put(prefix + "visibility", workbook.getSheetVisibility(i).name());
            result.put(prefix + "merges", sheet.getMergedRegions().toString());
            for (int c = 0; c < 256; c++) {
                result.put(prefix + "col:" + c, sheet.getColumnWidth(c) + ":" + sheet.isColumnHidden(c));
            }
            for (Row row : sheet) {
                for (Cell value : row) {
                    HSSFCell cell = (HSSFCell)value;
                    String key = sheet.getSheetName() + "!" + cell.getAddress().formatAsString();
                    result.put(key + ":value", cell.getCellType() + ":" + raw(cell, cell.getCellType()));
                    result.put(key + ":style", cell.getCellStyle().getIndex() + ":" + row.getHeight() + ":" + row.getZeroHeight());
                    if (cell.getCellComment() != null) result.put(key + ":comment", cell.getCellComment().getString().getString());
                    if (cell.getHyperlink() != null) result.put(key + ":hyperlink", cell.getHyperlink().getAddress());
                }
            }
        }
        return result;
    }

    private static void verifyPreserved(Map<String, String> before, Map<String, String> after, Set<String> targets) {
        for (Map.Entry<String, String> entry : before.entrySet()) {
            String key = entry.getKey();
            if (key.endsWith(":value") && targets.contains(key.substring(0, key.length() - 6))) continue;
            require(Objects.equals(entry.getValue(), after.get(key)), "preservation mismatch: " + key);
        }
    }

    private static CellReference reference(String value) {
        require(value.matches("[A-Za-z]{1,2}[1-9][0-9]{0,4}"), "cell requires an XLS A1 address");
        CellReference result = new CellReference(value.toUpperCase(Locale.ROOT));
        require(result.getRow() < 65536 && result.getCol() < 256, "cell outside XLS bounds");
        return result;
    }

    private static HSSFSheet sheet(HSSFWorkbook workbook, String name) {
        HSSFSheet result = workbook.getSheet(name);
        require(result != null, "sheet not found: " + name);
        return result;
    }

    private static String text(JsonObject input, String name) {
        require(input.has(name) && input.get(name).isJsonPrimitive()
            && input.get(name).getAsJsonPrimitive().isString(), "missing string field: " + name);
        return input.get(name).getAsString();
    }

    private static void require(boolean condition, String message) {
        if (!condition) throw new IllegalArgumentException(message);
    }
}
