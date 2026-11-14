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
/// launch, run, install or import something is shown in Finder instead of
/// opened (ADR 0027 amendments, #299, #437). That is judged by what the file
/// is and by what would open it, not by a list of names:
///
/// - Launch Services' own risk list (``riskList``: the
///   `LSRiskCategoryUnsafeExecutable` types and extensions, and the
///   installer packages of `LSRiskCategoryMayContainUnsafeExecutable`), read
///   from the system at first use and joined to a built-in copy;
/// - the app that would open it (``isRiskyHandler(_:)``): Terminal, Installer,
///   Keychain Access, Shortcuts, Script Editor, System Settings and the other
///   system apps that run, install or import what they open, and any agent
///   app (no Dock icon: ProfileHelper, DiskImageMounter, Automator Installer,
///   a third-party plug-in installer), which handles a file rather than
///   showing it;
/// - a per-file `com.apple.LaunchServices.OpenWith` attribute, which hands
///   the file to any app it names;
/// - what the file is: an application or other package, a Finder alias or
///   symbolic link (Foundation counts both as aliases), an executable or
///   script, a stored URL (handed to its scheme's handler, whatever the
///   scheme, which would get around the scheme rule above), a disk image, a
///   profile, certificate, shortcut or workflow (``isLaunchable(_:)``).
///
/// Every rule judges the path as given and as resolved (a directory on the
/// way may be a link).
///
/// Any other local file or folder opens normally: an archive too, which
/// Archive Utility extracts without running anything.
///
/// The file system is consulted only for a click (``action(for:localHostName:fileManager:handler:)``):
/// the Command-hover asks ``allows(_:localHostName:)``, which reads the URL
/// alone, so a hung network mount cannot stall the pointer. A hovered
/// `file:` link to a path that does not exist is underlined, and a click
/// on it opens nothing and beeps.
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

    /// Kinds of file Launch Services itself counts as unsafe to open: type
    /// identifiers (a file conforming to one is shown) and lower-cased
    /// extensions.
    struct RiskList: Equatable, Sendable {
        var types: Set<String>
        var extensions: Set<String>

        func union(_ other: RiskList) -> RiskList {
            RiskList(types: types.union(other.types), extensions: extensions.union(other.extensions))
        }
    }

    /// Where Launch Services keeps its risk categories (CoreTypes).
    static let systemRiskListURL = URL(
        fileURLWithPath: "/System/Library/CoreServices/CoreTypes.bundle/Contents/Resources/System")
    /// The most of the system's risk list read (16 KB on macOS 27); a
    /// larger file is not read and the built-in copy stands alone.
    static let maxRiskListBytes = 1 << 20
    /// The archive type `LSRiskCategoryMayContainUnsafeExecutable` lists
    /// beside the installer packages: archives keep opening (Archive Utility
    /// extracts, it runs nothing).
    private static let archiveType = "public.archive"

    /// The risk list as macOS 27.2 has it, so a system whose file is missing,
    /// moved or unreadable still shows these. The test that reads the
    /// system's list fails when the system lists a kind this copy lacks.
    static let builtInRiskList = RiskList(
        types: [
            "com.apple.mach-o-binary", "com.apple.webarchive", "com.apple.application",
            "com.apple.interfacebuilder.document", "com.apple.terminal.session", "com.apple.terminal.settings",
            "com.apple.terminal.shell-script", "com.sun.java-class", "com.sun.java-archive", "com.sun.java-web-start",
            "com.apple.quartz-composer-composition", "public.unix-executable", "com.apple.itunes.ipg",
            "com.apple.installer-distribution-package", "com.apple.installer-package",
            "com.apple.installer-package-archive", "com.apple.itunes.ipa",
        ],
        extensions: [
            "cin", "command", "gcx", "help", "ibplugin", "icp", "inputplugin", "menu", "nib", "service", "term",
            "terminal", "tool", "wflow", "x11app", "ipg", "ipa",
        ])

    /// The risk list read from `data` (the system file's property list):
    /// every type and extension of `LSRiskCategoryUnsafeExecutable`, and
    /// those of `LSRiskCategoryMayContainUnsafeExecutable` but the archive
    /// type. `nil` when `data` is not a property list dictionary or names
    /// neither category.
    static func riskList(fromPropertyList data: Data) -> RiskList? {
        guard data.count <= maxRiskListBytes,
            let root = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any]
        else { return nil }
        var list = RiskList(types: [], extensions: [])
        var found = false
        for category in ["LSRiskCategoryUnsafeExecutable", "LSRiskCategoryMayContainUnsafeExecutable"] {
            guard let entries = root[category] as? [String: Any] else { continue }
            found = true
            func strings(_ key: String) -> [String] {
                (entries[key] as? [Any] ?? []).compactMap { $0 as? String }
            }
            list.types.formUnion(strings("LSRiskCategoryContentTypes").filter { $0 != archiveType })
            list.extensions.formUnion(strings("LSRiskCategoryExtensions").map { $0.lowercased() })
        }
        return found ? list : nil
    }

    /// The system's risk list, or `nil` when its file cannot be read.
    static func systemRiskList(at url: URL = systemRiskListURL) -> RiskList? {
        guard let size = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize, size <= maxRiskListBytes,
            let data = try? Data(contentsOf: url)
        else { return nil }
        return riskList(fromPropertyList: data)
    }

    /// What the policy shows by Launch Services' judgement: the system's
    /// list, read once, with the built-in copy.
    static let riskList: RiskList = builtInRiskList.union(systemRiskList() ?? builtInRiskList)

    /// By type, beside the risk list, so a renamed file is caught too: every
    /// executable and script, every stored URL (`public.stored-url`: `.url`,
    /// `.webloc`, `.vncloc`, `.afploc` and the rest), every disk image
    /// (`.img` and `.iso` as well as `.dmg`), and what installs or starts a
    /// flow that does whatever app is bound to it: configuration and
    /// provisioning profiles (Xcode, not ProfileHelper, opens a
    /// `.mobileprovision` and installs it), X.509 certificates and PKCS #12
    /// bundles, keychains, shortcuts in either format, Java Web Start files,
    /// Automator workflows and Safari extensions.
    private static let launchableTypes: [UTType] =
        [.executable, .script, .shellScript, .applicationBundle, .unixExecutable, .diskImage]
        + ([
            "public.stored-url", "com.apple.mobileconfig", "com.apple.configprofile", "com.apple.provisionprofile",
            "com.apple.mobileprovision", "public.x509-certificate", "com.rsa.pkcs-12", "com.apple.keychain",
            "com.apple.shortcut", "com.apple.shortcuts.workflow-file", "com.sun.java-web-start",
            "com.apple.automator-workflow", "com.apple.safari.extension",
        ] + riskList.types.sorted()).compactMap { UTType($0) }

    /// Whether a file of `type` is shown rather than opened (tests: every
    /// rule of ``launchableTypes`` on its own).
    static func isLaunchable(_ type: UTType) -> Bool {
        launchableTypes.contains { type.conforms(to: $0) }
    }

    /// System apps that run, install or import what they open, by bundle
    /// identifier (lower-cased): what opening a file with one of them does
    /// is not showing it. Agent apps are caught without a name
    /// (``isRiskyHandler(_:)``).
    static let riskyHandlers: Set<String> = [
        "com.apple.terminal", "com.apple.installer", "com.apple.mcx.profilehelper", "com.apple.shortcuts",
        "com.apple.automatorinstaller", "com.apple.automator", "com.apple.keychainaccess", "com.apple.scripteditor2",
        "com.apple.javalauncher", "com.apple.systempreferences", "com.apple.helpviewer", "com.apple.systemuiserver",
        "com.apple.diskimagemounter", "com.apple.calendarfilehandler", "com.apple.ical", "com.apple.addressbook",
    ]

    /// Whether opening a file with the app at `app` could run, install or
    /// import it: one of ``riskyHandlers``, or an agent app (`LSUIElement`
    /// or `LSBackgroundOnly`), which has no window to show a document in
    /// and handles the file instead. An app whose bundle cannot be read is
    /// judged by nothing (the other rules still apply).
    static func isRiskyHandler(_ app: URL) -> Bool {
        guard let bundle = Bundle(url: app) else { return false }
        if let identifier = bundle.bundleIdentifier?.lowercased(), riskyHandlers.contains(identifier) { return true }
        let info = bundle.infoDictionary ?? [:]
        return isTrue(info["LSUIElement"]) || isTrue(info["LSBackgroundOnly"])
    }

    /// The default handler Launch Services names for `url`, as
    /// `NSWorkspace.open` would use it.
    static func defaultHandler(for url: URL) -> URL? {
        NSWorkspace.shared.urlForApplication(toOpen: url)
    }

    /// The extended attribute that binds one file to an app, whatever its
    /// type's default (Finder's "Open With" for one file; a program can set
    /// it too, and it survives a tar round trip).
    static let openWithAttribute = "com.apple.LaunchServices.OpenWith"

    /// This Mac's host name, as `gethostname` gives it.
    static var localHostName: String {
        var buffer = [CChar](repeating: 0, count: 256)
        guard gethostname(&buffer, buffer.count) == 0 else { return "" }
        return String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    /// What to do for `string`, or `nil` when nothing may be done. A local
    /// `file:` URL is looked up on disk: call this for a click only.
    /// `handler` names the app that would open a file (tests pass their
    /// own; the default asks Launch Services).
    static func action(
        for string: String, localHostName: String = LinkPolicy.localHostName,
        fileManager: FileManager = .default, handler: (URL) -> URL? = LinkPolicy.defaultHandler(for:)
    ) -> Action? {
        guard let (url, isFile) = allowedURL(string, localHostName: localHostName) else { return nil }
        guard isFile else { return .open(url) }
        guard fileManager.fileExists(atPath: url.path) else { return nil }
        return launches(url, fileManager: fileManager, handler: handler) ? .reveal(url) : .open(url)
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

    /// Whether opening `url` could launch, run, install or import something:
    /// judged on the path itself and on the path with its links resolved.
    private static func launches(_ url: URL, fileManager: FileManager, handler: (URL) -> URL?) -> Bool {
        let targets = [url, url.resolvingSymlinksInPath()]
        return targets.contains { target in
            if riskList.extensions.contains(target.pathExtension.lowercased()) { return true }
            if hasOpenWith(target) { return true }
            let values = try? target.resourceValues(forKeys: [
                .isApplicationKey, .isPackageKey, .isDirectoryKey, .contentTypeKey, .isAliasFileKey,
            ])
            // A Finder alias leads anywhere (an app, a script) with no sign of
            // it in its name or mode: every one is shown, not opened, and so
            // is every symbolic link (Foundation counts it as an alias).
            if values?.isAliasFile == true { return true }
            if values?.isApplication == true || values?.isPackage == true { return true }
            if let type = values?.contentType, isLaunchable(type) { return true }
            if let app = handler(target), isRiskyHandler(app) { return true }
            // Directories are "executable" for the search permission only.
            return values?.isDirectory != true && fileManager.isExecutableFile(atPath: target.path)
        }
    }

    /// Whether the file at `url` carries ``openWithAttribute``.
    private static func hasOpenWith(_ url: URL) -> Bool {
        url.withUnsafeFileSystemRepresentation { path in
            guard let path else { return false }
            return getxattr(path, openWithAttribute, nil, 0, 0, 0) >= 0
        }
    }

    /// An `Info.plist` flag: a Boolean (or the number 0 or 1, which
    /// bridges to one) or the strings property lists use for one.
    private static func isTrue(_ value: Any?) -> Bool {
        switch value {
        case let flag as Bool: flag
        case let text as String: ["1", "true", "yes"].contains(text.lowercased())
        default: false
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
