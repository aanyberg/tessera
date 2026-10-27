import Foundation
internal import TesseraCore

extension TesseraSession {
    /// What ``TesseraApp/spawn(_:)`` starts. The defaults run the user's login
    /// shell in the home directory at 80 x 24.
    public struct SpawnOptions: Sendable, Equatable {
        /// Program to run: an absolute path, or a name looked up in `PATH`.
        /// `nil` runs the user's login shell (`$SHELL`) as a login shell.
        public var program: String?
        /// The program's arguments, excluding `argv[0]`. Ignored for the login
        /// shell.
        public var arguments: [String]
        /// Working directory, or `nil` for the home directory. Must be a file
        /// URL. A path that is not an existing directory falls back to the home
        /// directory.
        public var workingDirectory: URL?
        /// Initial size in cells; both must be at least 1.
        public var rows: UInt16
        public var cols: UInt16
        /// Size of the view in pixels, or 0 when unknown.
        public var widthPx: UInt16
        public var heightPx: UInt16

        public init(
            program: String? = nil,
            arguments: [String] = [],
            workingDirectory: URL? = nil,
            rows: UInt16 = 24,
            cols: UInt16 = 80,
            widthPx: UInt16 = 0,
            heightPx: UInt16 = 0
        ) {
            self.program = program
            self.arguments = arguments
            self.workingDirectory = workingDirectory
            self.rows = rows
            self.cols = cols
            self.widthPx = widthPx
            self.heightPx = heightPx
        }

        /// Calls `body` with the C options. Every pointer in them is valid only
        /// inside `body`, matching the header's "borrowed for the duration of
        /// the call" rule.
        ///
        /// - Throws: ``TesseraCoreError/invalidArgument`` if a string contains a
        ///   NUL byte, which a C string cannot carry, or if `workingDirectory`
        ///   is not a file URL (its path would name nothing on disk).
        func withCOptions<R>(_ body: (inout TesseraSpawnOptions) -> R) throws -> R {
            let strings = [program].compactMap { $0 } + arguments
            if strings.contains(where: { $0.utf8.contains(0) }) {
                throw TesseraCoreError.invalidArgument
            }
            if let workingDirectory, !workingDirectory.isFileURL {
                throw TesseraCoreError.invalidArgument
            }
            return withOptionalCString(program) { programPointer in
                withCStringArray(arguments) { argPointers in
                    withOptionalFileSystemRepresentation(workingDirectory) { cwdPointer in
                        argPointers.withUnsafeBufferPointer { args in
                            var options = TesseraSpawnOptions(
                                program: programPointer,
                                args: args.baseAddress,
                                arg_count: args.count,
                                cwd: cwdPointer,
                                rows: rows,
                                cols: cols,
                                width_px: widthPx,
                                height_px: heightPx
                            )
                            return body(&options)
                        }
                    }
                }
            }
        }
    }
}

private func withOptionalCString<R>(
    _ string: String?, _ body: (UnsafePointer<CChar>?) -> R
) -> R {
    guard let string else { return body(nil) }
    return string.withCString { body($0) }
}

private func withOptionalFileSystemRepresentation<R>(
    _ url: URL?, _ body: (UnsafePointer<CChar>?) -> R
) -> R {
    guard let url else { return body(nil) }
    return url.withUnsafeFileSystemRepresentation { body($0) }
}

/// Calls `body` with pointers to NUL-terminated copies of `strings`, all held
/// in one buffer that lives until `body` returns.
private func withCStringArray<R>(
    _ strings: [String], _ body: ([UnsafePointer<CChar>?]) -> R
) -> R {
    var storage: [CChar] = []
    var offsets: [Int] = []
    for string in strings {
        offsets.append(storage.count)
        storage.append(contentsOf: string.utf8CString)
    }
    return storage.withUnsafeBufferPointer { buffer in
        // `buffer.baseAddress` is nil only when `strings` is empty.
        let pointers = offsets.map { offset in buffer.baseAddress.map { $0 + offset } }
        return body(pointers)
    }
}
