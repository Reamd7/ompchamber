import SwiftUI
import WidgetKit

// MARK: - Shared model + App Group reader

/// One row of the session overview the app writes to the shared App Group.
/// Mirrors MobileWidgetSession in packages/ui/src/apps/mobileWidgetSnapshot.ts.
struct WidgetSession: Codable, Identifiable, Hashable {
    let id: String
    let title: String
    let unread: Bool
    /// Project label for the session's directory. Optional so snapshots written before this
    /// field existed still decode.
    var project: String?
}

/// The session overview snapshot. Mirrors MobileWidgetSnapshot (same field names) so the
/// JSON the app stores decodes directly.
struct WidgetSnapshot: Codable {
    var runtimeKey: String?
    let attentionCount: Int
    let recentSessions: [WidgetSession]

    static let empty = WidgetSnapshot(runtimeKey: nil, attentionCount: 0, recentSessions: [])
}

enum WidgetStore {
    static let appGroup = "group.com.ompchamber.app"
    static let snapshotKey = "widgetSnapshot"

    /// Reads the latest snapshot the app persisted. Returns `.empty` when nothing has been
    /// written yet (fresh install / app never foregrounded) so widgets render a clean state.
    static func load() -> WidgetSnapshot {
        guard let defaults = UserDefaults(suiteName: appGroup),
              let json = defaults.string(forKey: snapshotKey),
              let data = json.data(using: .utf8),
              let snapshot = try? JSONDecoder().decode(WidgetSnapshot.self, from: data) else {
            return .empty
        }
        return snapshot
    }
}

// MARK: - Deep links (mirror packages/ui/src/apps/deepLinks.ts)

enum WidgetDeepLink {
    static func newSession() -> URL { URL(string: "ompchamber://new")! }
    static func attention() -> URL { URL(string: "ompchamber://sessions?filter=attention")! }
    static func status() -> URL { URL(string: "ompchamber://status")! }
    static func settings() -> URL { URL(string: "ompchamber://settings")! }
    static func changes() -> URL { URL(string: "ompchamber://changes")! }
    static func files() -> URL { URL(string: "ompchamber://view/files")! }
    static func instances() -> URL { URL(string: "ompchamber://view/instances")! }
    static func session(_ id: String) -> URL {
        let encoded = id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? id
        return URL(string: "ompchamber://session/\(encoded)") ?? newSession()
    }
}

// MARK: - Timeline provider

struct OverviewEntry: TimelineEntry {
    let date: Date
    let snapshot: WidgetSnapshot
}

struct OverviewProvider: TimelineProvider {
    func placeholder(in context: Context) -> OverviewEntry {
        OverviewEntry(date: Date(), snapshot: .empty)
    }

    func getSnapshot(in context: Context, completion: @escaping (OverviewEntry) -> Void) {
        completion(OverviewEntry(date: Date(), snapshot: WidgetStore.load()))
    }

    func getTimeline(in context: Context, completion: @escaping (Timeline<OverviewEntry>) -> Void) {
        // The app/NSE reload timelines (WidgetCenter) when the snapshot changes, but with several
        // widgets sharing the app's WidgetKit reload budget iOS can refresh them unevenly and
        // leave one stale. Ask for a periodic refresh too so every widget independently re-reads
        // the shared snapshot and converges to the latest state (budget permitting).
        let entry = OverviewEntry(date: Date(), snapshot: WidgetStore.load())
        let nextRefresh = Date().addingTimeInterval(10 * 60)
        completion(Timeline(entries: [entry], policy: .after(nextRefresh)))
    }
}

// MARK: - Logo (official Oh My Pi mark drawn from the SVG)

/// The OMPChamber logo, drawn to match the official Oh My Pi mark
/// (https://github.com/can1357/oh-my-pi/blob/main/assets/icon.svg): a π glyph built from
/// a top bar and two legs, with an orange plugin connector (two slots cut out via
/// even-odd) resting on the right leg, plus two accent dots on the bar. The π bars use
/// `.primary` so the system tint on the Lock Screen / Control Center colours them, while
/// the connector keeps the official orange. Coordinates are the SVG source units
/// (range x:10…110, y:8…82 in a 120×90 system).
struct OmpLogoView: View {
    var body: some View {
        Canvas { context, size in
            let scale = min(size.width / 100, size.height / 74) * 0.98
            let ox = size.width / 2 - 60 * scale
            let oy = size.height / 2 - 45 * scale
            // SVG source coordinate → canvas point.
            func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: ox + x * scale, y: oy + y * scale) }
            func bar(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) -> Path {
                Path(roundedRect: CGRect(x: ox + x * scale, y: oy + y * scale, width: w * scale, height: h * scale), cornerRadius: 2 * scale)
            }

            // π: top bar + two legs.
            context.fill(bar(10, 8, 100, 12), with: .color(.primary))
            context.fill(bar(25, 20, 12, 62), with: .color(.primary))
            context.fill(bar(75, 20, 12, 45), with: .color(.primary))

            // Orange plugin connector with two slots cut out (even-odd).
            var connector = Path()
            connector.addRoundedRect(in: CGRect(x: ox + 71 * scale, y: oy + 55 * scale, width: 20 * scale, height: 16 * scale), cornerSize: CGSize(width: 3 * scale, height: 3 * scale))
            connector.addRoundedRect(in: CGRect(x: ox + 76 * scale, y: oy + 59 * scale, width: 3 * scale, height: 8 * scale), cornerSize: CGSize(width: 1 * scale, height: 1 * scale))
            connector.addRoundedRect(in: CGRect(x: ox + 82 * scale, y: oy + 59 * scale, width: 3 * scale, height: 8 * scale), cornerSize: CGSize(width: 1 * scale, height: 1 * scale))
            context.fill(connector, with: .color(.orange), style: FillStyle(eoFill: true))

            // Two accent dots on the top bar.
            for dotX in [18, 102] {
                let c = p(dotX, 14)
                let dot = Path(ellipseIn: CGRect(x: c.x - 2 * scale, y: c.y - 2 * scale, width: 4 * scale, height: 4 * scale))
                context.fill(dot, with: .color(.orange.opacity(0.8)))
            }
        }
    }
}
