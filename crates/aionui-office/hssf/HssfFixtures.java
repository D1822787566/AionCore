import java.io.*;
import java.nio.file.*;
import java.time.LocalDateTime;
import org.apache.poi.hssf.usermodel.*;
import org.apache.poi.hssf.record.DateWindow1904Record;
import org.apache.poi.ss.usermodel.*;
import org.apache.poi.ss.util.*;

/** Test inputs only; this class is not included in the app's runtime bundle. */
public final class HssfFixtures {
    public static void main(String[] args) throws Exception {
        Path directory = Path.of(args[0]);
        Files.createDirectories(directory);
        for (String kind : new String[]{"normal", "date1904", "protected", "array", "drawing", "unsupported"}) {
            try (HSSFWorkbook book = new HSSFWorkbook()) {
                HSSFSheet sheet = book.createSheet("报表");
                HSSFRow row = sheet.createRow(0);
                row.createCell(0).setCellValue("00123");
                row.createCell(1).setCellValue(12.5);
                row.createCell(2).setCellFormula(kind.equals("unsupported") ? "FPO_CUSTOM(B1)" : "B1*2");
                HSSFCellStyle style = book.createCellStyle();
                style.setDataFormat(book.createDataFormat().getFormat("yyyy-mm-dd"));
                HSSFCell date = row.createCell(3);
                date.setCellStyle(style);
                if (kind.equals("date1904")) {
                    ((DateWindow1904Record) book.getInternalWorkbook()
                        .findFirstRecordBySid(DateWindow1904Record.sid)).setWindowing((short)1);
                }
                date.setCellValue(LocalDateTime.of(2026, 1, 1, 0, 0));
                sheet.createRow(2).createCell(0).setCellValue("merged");
                sheet.addMergedRegion(new CellRangeAddress(2, 2, 0, 1));
                if (kind.equals("protected")) sheet.protectSheet("test-password");
                if (kind.equals("array")) sheet.setArrayFormula("B1*2", new CellRangeAddress(4, 5, 0, 0));
                if (kind.equals("drawing")) {
                    HSSFPatriarch drawing = sheet.createDrawingPatriarch();
                    drawing.createSimpleShape(new HSSFClientAnchor(0,0,0,0,(short)0,7,(short)2,9))
                        .setShapeType(HSSFSimpleShape.OBJECT_TYPE_RECTANGLE);
                }
                if (!kind.equals("unsupported")) book.getCreationHelper().createFormulaEvaluator().evaluateAll();
                try (OutputStream output = Files.newOutputStream(directory.resolve(kind + ".xls"))) { book.write(output); }
            }
        }
        System.out.println("HSSF_FIXTURES_CREATED count=6");
    }
}
