import AppKit
import Darwin
import UniformTypeIdentifiers

/// What a command-click does with a link: opens it, or only shows it in
/// Finder.
@MainActor
protocol LinkOpening: AnyObject {
    func open(_ url: URL)
    /// Selects the file in Finder without launching it.
    func reveal(_ url: URL)
}

/// Production opener: the URL's default handler, or Finder, through
/// `NSWorkspace`.
@MainActor
final class WorkspaceLinkOpener: LinkOpening {
    func open(_ url: URL) {
        NSWorkspace.shared.open(url)
    }

    func reveal(_ url: URL) {
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}

/// Which links a command-click may open (D-OPEN): `http`, `https`, `mailto`
/// and `ftp`, and `file` only for a host that is empty, `localhost` or this
/// Mac's own, and only for a path that exists. Everything else
/// (`javascript:`, `vscode:`, a `file:` URL on another host, text
/// `URL(string:)` rejects) does nothing. One rule for the URLs the core
/// detects and the ones a program writes with OSC 8.
///
/// Link text is output a program controls, so a local `file:` URL that would
/// launch something (an application or other bundle, an executable or
/// script, an installer package or disk image, a Java archive, a `.command`,
/// `.webloc`, `.url` or other stored URL, a Finder alias) is shown in Finder
/// instead of opened; any other local file or folder opens normally. A
/// stored URL is handed to its scheme's handler, whatever the scheme, so
/// opening one would get around the scheme rule above.
///
/// The file system is consulted only for a click (``action(for:localHostName:fileManager:)``):
/// the Command-hover asks ``allows(_:localHostName:)``, which reads the URL
/// alone, so a hung network mount cannot stall the pointer. A hovered
/// `file:` link to a path that does not exist is underlined and does
/// nothing when clicked.
enum LinkPolicy {
    enum Action: Equatable {
        case open(URL)
        case reveal(URL)

        var url: URL {
            switch self {
            case .open(let url), .reveal(let url): url
            }
        }
    }

    private static let remoteSchemes: Set<String> = ["http", "https", "mailto", "ftp"]
    /// Files the system would launch or run, or that redirect to something
    /// that does.
    private static let launchableExtensions: Set<String> = [
        "command", "tool", "terminal", "webloc", "inetloc", "fileloc", "pkg", "mpkg", "dmg", "jar",
    ]
    /// By type, so a renamed file is caught too: every stored URL
    /// (`public.stored-url`: `.url`, `.webloc`, `.vncloc`, `.afploc` and the
    /// rest) and every disk image (`.img` and `.iso` as well as `.dmg`).
    private static let launchableTypes: [UTType] =
        [.executable, .script, .shellScript, .applicationBundle, .unixExecutable, .diskImage]
        + [UTType("public.stored-url")].compactMap { $0 }

    /// This Mac's host name, as `gethostname` gives it.
    static var localHostName: String {
        var buffer = [CChar](repeating: 0, count: 256)
        guard gethostname(&buffer, buffer.count) == 0 else { return "" }
        return String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    /// What to do for `string`, or `nil` when nothing may be done. A local
    /// `file:` URL is looked up on disk: call this for a click only.
    static func action(
        for string: String, localHostName: String = LinkPolicy.localHostName,
        fileManager: FileManager = .default
    ) -> Action? {
        guard let (url, isFile) = allowedURL(string, localHostName: localHostName) else { return nil }
        guard isFile else { return .open(url) }
        guard fileManager.fileExists(atPath: url.path) else { return nil }
        return launches(url, fileManager: fileManager) ? .reveal(url) : .open(url)
    }

    /// Whether a click on `string` may do something, judged on the URL alone
    /// (its scheme and, for `file:`, its host): the file system is not
    /// touched. The Command-hover's test.
    static func allows(_ string: String, localHostName: String = LinkPolicy.localHostName) -> Bool {
        allowedURL(string, localHostName: localHostName) != nil
    }

    /// The URL `string` names when its scheme and host allow it, and whether
    /// it is a local `file:` URL.
    private static func allowedURL(_ string: String, localHostName: String) -> (URL, isFile: Bool)? {
        guard let url = URL(string: string), let scheme = url.scheme?.lowercased() else { return nil }
        if remoteSchemes.contains(scheme) { return (url, false) }
        guard scheme == "file" else { return nil }
        let host = normalised(url.host(percentEncoded: false) ?? "")
        let local = normalised(localHostName)
        guard host.isEmpty || host == "localhost" || (!local.isEmpty && host == local) else { return nil }
        return (url, true)
    }

    /// Whether opening `url` could launch or run something: judged on the
    /// path itself and on what a symbolic link leads to.
    private static func launches(_ url: URL, fileManager: FileManager) -> Bool {
        let targets = [url, url.resolvingSymlinksInPath()]
        return targets.contains { target in
            if launchableExtensions.contains(target.pathExtension.lowercased()) { return true }
            let values = try? target.resourceValues(forKeys: [
                .isApplicationKey, .isPackageKey, .isDirectoryKey, .contentTypeKey, .isAliasFileKey,
            ])
            // A Finder alias leads anywhere (an app, a script) with no sign of
            // it in its name or mode: every one is shown, not opened.
            if values?.isAliasFile == true { return true }
            if values?.isApplication == true || values?.isPackage == true { return true }
            if let type = values?.contentType, launchableTypes.contains(where: { type.conforms(to: $0) }) {
                return true
            }
            // Directories are "executable" for the search permission only.
            return values?.isDirectory != true && fileManager.isExecutableFile(atPath: target.path)
        }
    }

    private static func normalised(_ host: String) -> String {
        var host = host.lowercased()
        while host.hasSuffix(".") {
            host.removeLast()
        }
        return host
    }
}
