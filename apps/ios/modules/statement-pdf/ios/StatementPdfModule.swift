import ExpoModulesCore

public final class StatementPdfModule: Module {
    public func definition() -> ModuleDefinition {
        Name("StatementPdf")

        AsyncFunction("read") { (uri: String) in
            try StatementPdfReader.read(uri: uri)
        }
    }
}
