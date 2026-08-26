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

// MARK: - Logo (full OpenChamber mark drawn from the SVG)

/// The OpenChamber logo, drawn to match packages/web/public/logo-dark-512x512.svg: an
/// isometric cube with translucent face fills, stroked edges, and the Oh my Pi mark on the
/// top face (π with an orange plugin connector). Faces use low-opacity `.primary` so the
/// system tint on the Lock Screen / Control Center reads as a translucent fill (no colour)
/// rather than a flat wireframe. Coordinates are
/// the SVG inner group (range x:-41.568…41.568, y:-48…48).
struct CubeLogoView: View {
    var body: some View {
        Canvas { context, size in
            let halfW: CGFloat = 41.568
            let halfH: CGFloat = 48
            let scale = min(size.width / (halfW * 2), size.height / (halfH * 2))
            let cx = size.width / 2
            let cy = size.height / 2
            let lineWidth = max(1.5, 3 * scale)

            // Cube coordinate → canvas point.
            func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: cx + x * scale, y: cy + y * scale) }
            // π-mark local coordinate → canvas point (SVG: matrix(0.866,0.5,-0.866,0.5,0,-24)).
            func m(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
                let mx = 0.866 * x - 0.866 * y
                let my = 0.5 * x + 0.5 * y - 24
                return p(mx, my)
            }

            var left = Path()
            left.move(to: p(0, 0)); left.addLine(to: p(-halfW, -24)); left.addLine(to: p(-halfW, 24)); left.addLine(to: p(0, 48)); left.closeSubpath()
            var right = Path()
            right.move(to: p(0, 0)); right.addLine(to: p(halfW, -24)); right.addLine(to: p(halfW, 24)); right.addLine(to: p(0, 48)); right.closeSubpath()
            var top = Path()
            top.move(to: p(0, -48)); top.addLine(to: p(-halfW, -24)); top.addLine(to: p(0, 0)); top.addLine(to: p(halfW, -24)); top.closeSubpath()

            context.fill(left, with: .color(.primary.opacity(0.2)))
            context.fill(right, with: .color(.primary.opacity(0.35)))
            context.stroke(left, with: .color(.primary), style: StrokeStyle(lineWidth: lineWidth, lineJoin: .round))
            context.stroke(right, with: .color(.primary), style: StrokeStyle(lineWidth: lineWidth, lineJoin: .round))
            context.stroke(top, with: .color(.primary), style: StrokeStyle(lineWidth: lineWidth, lineJoin: .round))

            // Oh my Pi mark: π bar + legs, orange plugin connector with two slots cut out
            // (even-odd), and two accent dots on the bar.
            var pi = Path()
            pi.move(to: m(-30, -22.2)); pi.addLine(to: m(30, -22.2)); pi.addLine(to: m(30, -15)); pi.addLine(to: m(-30, -15)); pi.closeSubpath()
            pi.move(to: m(-21, -15)); pi.addLine(to: m(-13.8, -15)); pi.addLine(to: m(-13.8, 22.2)); pi.addLine(to: m(-21, 22.2)); pi.closeSubpath()
            pi.move(to: m(9, -15)); pi.addLine(to: m(16.2, -15)); pi.addLine(to: m(16.2, 12)); pi.addLine(to: m(9, 12)); pi.closeSubpath()
            context.fill(pi, with: .color(.primary))

            var connector = Path()
            connector.move(to: m(6.6, 6)); connector.addLine(to: m(18.6, 6)); connector.addLine(to: m(18.6, 15.6)); connector.addLine(to: m(6.6, 15.6)); connector.closeSubpath()
            connector.move(to: m(9.6, 8.4)); connector.addLine(to: m(9.6, 13.2)); connector.addLine(to: m(11.4, 13.2)); connector.addLine(to: m(11.4, 8.4)); connector.closeSubpath()
            connector.move(to: m(13.2, 8.4)); connector.addLine(to: m(13.2, 13.2)); connector.addLine(to: m(15, 13.2)); connector.addLine(to: m(15, 8.4)); connector.closeSubpath()
            context.fill(connector, with: .color(.orange), style: FillStyle(eoFill: true))

            for dotX in [-25.2, 25.2] {
                let c = m(dotX, -18.6)
                let dot = Path(ellipseIn: CGRect(x: c.x - 1.2 * scale, y: c.y - 1.2 * scale, width: 2.4 * scale, height: 2.4 * scale))
                context.fill(dot, with: .color(.orange.opacity(0.8)))
            }
        }
    }
}
