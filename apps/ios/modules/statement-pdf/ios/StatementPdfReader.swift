import CryptoKit
import Foundation
import PDFKit

enum StatementPdfReader {
    static func read(uri: String) throws -> [String: String] {
        guard let url = URL(string: uri), url.isFileURL else {
            throw PdfError("Choose a local PDF file.")
        }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        let size = try url.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
        guard size > 0 && size <= 25 * 1024 * 1024 else {
            throw PdfError("Choose a PDF smaller than 25 MB.")
        }
        let data = try Data(contentsOf: url, options: [.mappedIfSafe])
        guard let document = PDFDocument(data: data), !document.isLocked,
              document.pageCount > 0, document.pageCount <= 100 else {
            throw PdfError("Choose an unlocked PDF with at most 100 pages.")
        }
        let text = (0..<document.pageCount)
            .compactMap { document.page(at: $0)?.string }
            .joined(separator: "\n")
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            throw PdfError("This PDF has no selectable text. Scanned statements are not supported.")
        }
        let hash = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        return ["text": text, "id": hash]
    }
}

private struct PdfError: LocalizedError {
    let message: String
    init(_ message: String) { self.message = message }
    var errorDescription: String? { message }
}
