use freya_builder::documents::read_folder;

#[test]
fn original_pdf_folder_is_ingested_without_a_manual_companion() {
    let dir = tempfile::tempdir().unwrap();
    let stream = "BT /F1 12 Tf 72 720 Td (Generic document text) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream),
    ];
    let mut pdf = "%PDF-1.4\n".to_string();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, object));
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    std::fs::write(dir.path().join("notes.pdf"), pdf).unwrap();
    let folder = read_folder(dir.path()).unwrap();
    assert_eq!(folder.documents.len(), 1);
    assert_eq!(folder.documents[0].kind, "untrusted_pdf_text");
    assert!(
        folder.documents[0]
            .content
            .as_str()
            .unwrap()
            .contains("Generic document text")
    );
}
