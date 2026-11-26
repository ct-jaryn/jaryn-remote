#import <AppKit/AppKit.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <float.h>

// ---------------------------------------------------------------------------
// 设计语言：链路台（Link Deck）
//
// 产品是「一个人自己的设备舰队」，界面围绕两件事组织：这台机器是谁，到对面
// 那台机器的链路通不通。视觉记忆点取自产品自己的图标——两块相连的屏幕、
// 一笔青绿色的 J——这里用它的青绿「链路色」和双屏标记，其余交给 macOS 原生
// 材质与系统文字色，保证明暗外观、键盘焦点和辅助功能都正常。
//
// 机器身份（设备短号、信令地址）一律等宽字；状态永远由文字 + 形状 + 颜色共同
// 表达，不靠颜色单独承担信息。
// ---------------------------------------------------------------------------

typedef void (*jrp_ui_action)(uint8_t kind, const char *arg, void *ctx);
void jrp_ui_show_logs(const char *dir);

// 动作编号必须和 apps/node/src/ui.rs 里的 ACT_* 一致。
enum {
    JR_ACT_INVITE = 1,
    JR_ACT_GRANT = 2,
    JR_ACT_REVOKE = 3,
    JR_ACT_AUTOSTART = 4,
    JR_ACT_END = 5,
    JR_ACT_RESCAN = 6,
    JR_ACT_VIEW = 8,
    JR_ACT_QUIT = 9,
    JR_ACT_PERMIT = 10,
    JR_ACT_LOGS = 11,
    JR_ACT_UNGRANT = 12,
    JR_ACT_WAKE = 13,
};

// 设备行标志，和 ui.rs 里的 ROW_* 一致。
enum {
    JR_ROW_ONLINE = 1 << 0,
    JR_ROW_LAN = 1 << 1,
    JR_ROW_REVOKED = 1 << 2,
    JR_ROW_CAN_INVITE = 1 << 3,
    JR_ROW_CAN_GRANT = 1 << 4,
    JR_ROW_CAN_REVOKE = 1 << 5,
    JR_ROW_HAS_GRANT = 1 << 6,
};

// ---------------------------------------------------------------------------
// 颜色、字体与形状
// ---------------------------------------------------------------------------

static BOOL jr_is_dark(NSAppearance *appearance) {
    NSAppearanceName name = [appearance bestMatchFromAppearancesWithNames:@[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]];
    return [name isEqualToString:NSAppearanceNameDarkAqua];
}

/// 链路色：取自产品图标里那笔青绿色的 J。
static NSColor *jr_link(void) {
    return [NSColor colorWithName:@"JRLink" dynamicProvider:^NSColor *(NSAppearance *appearance) {
        if (jr_is_dark(appearance)) {
            return [NSColor colorWithSRGBRed:0.36 green:0.85 blue:0.96 alpha:1.0];
        }
        return [NSColor colorWithSRGBRed:0.04 green:0.56 blue:0.73 alpha:1.0];
    }];
}

/// 薄荷色：链路色渐变的另一端，只用在选中条和标记上。
static NSColor *jr_mint(void) {
    return [NSColor colorWithName:@"JRMint" dynamicProvider:^NSColor *(NSAppearance *appearance) {
        if (jr_is_dark(appearance)) {
            return [NSColor colorWithSRGBRed:0.47 green:0.96 blue:0.87 alpha:1.0];
        }
        return [NSColor colorWithSRGBRed:0.05 green:0.60 blue:0.52 alpha:1.0];
    }];
}

/// 面板底色：亮色下是一块更亮的纸，暗色下是一层浅浅的提亮。不是浮起来的玻璃卡。
static NSColor *jr_surface(void) {
    return [NSColor colorWithName:@"JRSurface" dynamicProvider:^NSColor *(NSAppearance *appearance) {
        if (jr_is_dark(appearance)) {
            return [NSColor colorWithWhite:1.0 alpha:0.055];
        }
        return [NSColor colorWithWhite:1.0 alpha:0.72];
    }];
}

static NSColor *jr_hairline(void) {
    return [[NSColor labelColor] colorWithAlphaComponent:0.10];
}

static NSFont *jr_font(CGFloat size, NSFontWeight weight) {
    return [NSFont systemFontOfSize:size weight:weight];
}

static NSFont *jr_mono(CGFloat size, NSFontWeight weight) {
    return [NSFont monospacedSystemFontOfSize:size weight:weight];
}

/// 小节标题：小字号加字距，用来分层而不是装饰。
static void jr_set_section_text(NSTextField *view, NSString *text) {
    view.attributedStringValue = [[NSAttributedString alloc] initWithString:text attributes:@{
        NSFontAttributeName: jr_font(11, NSFontWeightSemibold),
        NSForegroundColorAttributeName: [NSColor tertiaryLabelColor],
        NSKernAttributeName: @0.9,
    }];
}

static NSTextField *jr_section_label(NSString *text) {
    NSTextField *view = [NSTextField labelWithString:text];
    jr_set_section_text(view, text);
    return view;
}

static NSTextField *jr_label(NSString *text, CGFloat size, NSFontWeight weight, NSColor *color) {
    NSTextField *view = [NSTextField labelWithString:text];
    view.font = jr_font(size, weight);
    view.textColor = color ?: [NSColor labelColor];
    view.lineBreakMode = NSLineBreakByTruncatingTail;
    return view;
}

static NSTextField *jr_mono_label(NSString *text, CGFloat size, NSFontWeight weight, NSColor *color) {
    NSTextField *view = [NSTextField labelWithString:text];
    view.font = jr_mono(size, weight);
    view.textColor = color ?: [NSColor secondaryLabelColor];
    view.lineBreakMode = NSLineBreakByTruncatingTail;
    return view;
}

/// 分割线用 NSBox 的 separator：跟随明暗外观，不用自己重算颜色。
static NSView *jr_rule(void) {
    NSBox *box = [[NSBox alloc] init];
    box.boxType = NSBoxSeparator;
    return box;
}

/// 从顶部量位置：在非翻转坐标系里写布局时用，读起来和设计稿一致。
static NSRect jr_top(CGFloat height, CGFloat top, CGFloat x, CGFloat width, CGFloat box) {
    return NSMakeRect(x, height - top - box, width, box);
}

/// A1B2C3D4 → A1B2 · C3D4，机器短号统一用等宽字显示。
static NSString *jr_format_code(NSString *short_id) {
    if (!short_id || short_id.length == 0) {
        return @"";
    }
    NSString *upper = short_id.uppercaseString;
    if (upper.length == 8) {
        return [NSString stringWithFormat:@"%@ · %@", [upper substringToIndex:4], [upper substringFromIndex:4]];
    }
    return upper;
}

static char *dup_c(const char *src) {
    if (!src) {
        return NULL;
    }
    size_t n = strlen(src);
    char *out = malloc(n + 1);
    if (!out) {
        return NULL;
    }
    memcpy(out, src, n + 1);
    return out;
}

typedef NS_ENUM(NSInteger, JrGlyph) {
    JrGlyphOnline,    // 实心点：在线
    JrGlyphLan,       // 实心点 + 外圈：局域网直连
    JrGlyphBusy,      // 空心环：会话中
    JrGlyphNeeds,     // 空心点 + 感叹：缺权限
    JrGlyphRevoked,   // 空心点 + 斜杠：已撤销
    JrGlyphOffline,   // 细空心圈：离线
    JrGlyphTerminal,  // 小方块：仅终端
};

static BOOL jr_contains(NSString *text, NSString *needle) {
    return [text rangeOfString:needle].location != NSNotFound;
}

static BOOL jr_needs_permission(NSString *status) {
    return jr_contains(status, @"需授权") || jr_contains(status, @"需重新允许") || jr_contains(status, @"安全提示");
}

static JrGlyph jr_glyph_for(NSString *status, unsigned flags) {
    if (jr_contains(status, @"已撤销") || (flags & JR_ROW_REVOKED)) return JrGlyphRevoked;
    if (jr_contains(status, @"仅终端")) return JrGlyphTerminal;
    if (jr_needs_permission(status)) return JrGlyphNeeds;
    if (jr_contains(status, @"忙")) return JrGlyphBusy;
    if (jr_contains(status, @"在线")) return (flags & JR_ROW_LAN) ? JrGlyphLan : JrGlyphOnline;
    return JrGlyphOffline;
}

static NSColor *jr_status_color(NSString *status, unsigned flags) {
    if (jr_contains(status, @"已撤销") || (flags & JR_ROW_REVOKED)) return [NSColor systemRedColor];
    if (jr_contains(status, @"仅终端")) return [NSColor secondaryLabelColor];
    if (jr_needs_permission(status)) return [NSColor systemOrangeColor];
    if (jr_contains(status, @"忙")) return jr_link();
    if (jr_contains(status, @"在线")) return [NSColor systemGreenColor];
    return [NSColor secondaryLabelColor];
}

static void jr_draw_glyph(NSRect box, JrGlyph kind, NSColor *color) {
    CGFloat s = box.size.width / 12.0;
    NSPoint c = NSMakePoint(NSMidX(box), NSMidY(box));
    NSRect dot = NSMakeRect(c.x - 3.4 * s, c.y - 3.4 * s, 6.8 * s, 6.8 * s);
    NSRect ring = NSInsetRect(box, 1.0 * s, 1.0 * s);
    [color setStroke];
    [color setFill];
    switch (kind) {
        case JrGlyphOnline:
            [[NSBezierPath bezierPathWithOvalInRect:dot] fill];
            break;
        case JrGlyphLan: {
            [[NSBezierPath bezierPathWithOvalInRect:dot] fill];
            NSBezierPath *outer = [NSBezierPath bezierPathWithOvalInRect:NSInsetRect(box, 0.9 * s, 0.9 * s)];
            outer.lineWidth = 1.1 * s;
            [[color colorWithAlphaComponent:0.55] setStroke];
            [outer stroke];
            break;
        }
        case JrGlyphBusy: {
            NSBezierPath *path = [NSBezierPath bezierPathWithOvalInRect:ring];
            path.lineWidth = 2.2 * s;
            [path stroke];
            break;
        }
        case JrGlyphNeeds: {
            NSBezierPath *path = [NSBezierPath bezierPathWithOvalInRect:ring];
            path.lineWidth = 1.2 * s;
            [path stroke];
            NSRect bar = NSMakeRect(c.x - 0.7 * s, c.y - 1.0 * s, 1.4 * s, 2.6 * s);
            [[NSBezierPath bezierPathWithRoundedRect:bar xRadius:0.7 * s yRadius:0.7 * s] fill];
            [[NSBezierPath bezierPathWithOvalInRect:NSMakeRect(c.x - 0.8 * s, c.y + 2.2 * s, 1.6 * s, 1.6 * s)] fill];
            break;
        }
        case JrGlyphRevoked: {
            NSBezierPath *path = [NSBezierPath bezierPathWithOvalInRect:ring];
            path.lineWidth = 1.2 * s;
            [path stroke];
            NSBezierPath *slash = [NSBezierPath bezierPath];
            [slash moveToPoint:NSMakePoint(c.x - 2.6 * s, c.y + 2.6 * s)];
            [slash lineToPoint:NSMakePoint(c.x + 2.6 * s, c.y - 2.6 * s)];
            slash.lineWidth = 1.4 * s;
            slash.lineCapStyle = NSLineCapStyleRound;
            [slash stroke];
            break;
        }
        case JrGlyphTerminal: {
            NSBezierPath *path = [NSBezierPath bezierPathWithRoundedRect:NSInsetRect(box, 1.0 * s, 1.6 * s)
                                                                xRadius:1.6 * s
                                                                yRadius:1.6 * s];
            path.lineWidth = 1.2 * s;
            [path stroke];
            NSBezierPath *prompt = [NSBezierPath bezierPath];
            [prompt moveToPoint:NSMakePoint(c.x - 1.9 * s, c.y - 1.2 * s)];
            [prompt lineToPoint:NSMakePoint(c.x - 0.7 * s, c.y)];
            [prompt lineToPoint:NSMakePoint(c.x - 1.9 * s, c.y + 1.2 * s)];
            prompt.lineWidth = 1.2 * s;
            prompt.lineCapStyle = NSLineCapStyleRound;
            [prompt stroke];
            NSBezierPath *line = [NSBezierPath bezierPath];
            [line moveToPoint:NSMakePoint(c.x + 0.3 * s, c.y + 1.2 * s)];
            [line lineToPoint:NSMakePoint(c.x + 1.9 * s, c.y + 1.2 * s)];
            line.lineWidth = 1.2 * s;
            line.lineCapStyle = NSLineCapStyleRound;
            [line stroke];
            break;
        }
        case JrGlyphOffline:
        default: {
            NSBezierPath *path = [NSBezierPath bezierPathWithOvalInRect:ring];
            path.lineWidth = 1.2 * s;
            [path stroke];
            break;
        }
    }
}

static NSPoint jr_pt(NSRect box, CGFloat nx, CGFloat ny) {
    return NSMakePoint(box.origin.x + nx * box.size.width, box.origin.y + ny * box.size.height);
}

/// 产品自己的图标：两块相连的屏幕 + 一笔青绿色的 J（可选底座）。
static void jr_draw_mark(NSRect box, BOOL with_stand) {
    CGFloat w = box.size.width;
    NSColor *link = jr_link();
    NSColor *ink = [NSColor labelColor];

    NSRect back = NSMakeRect(box.origin.x + 0.17 * w, box.origin.y + 0.324 * box.size.height,
                             0.50 * w, 0.344 * box.size.height);
    NSBezierPath *back_path = [NSBezierPath bezierPathWithRoundedRect:back xRadius:0.070 * w yRadius:0.070 * w];
    back_path.lineWidth = MAX(1.0, 0.030 * w);
    [[ink colorWithAlphaComponent:0.30] setStroke];
    [back_path stroke];

    NSRect front = NSMakeRect(box.origin.x + 0.279 * w, box.origin.y + 0.240 * box.size.height,
                              0.549 * w, 0.402 * box.size.height);
    NSBezierPath *front_path = [NSBezierPath bezierPathWithRoundedRect:front xRadius:0.072 * w yRadius:0.072 * w];
    [[link colorWithAlphaComponent:0.14] setFill];
    [front_path fill];
    front_path.lineWidth = MAX(1.2, 0.038 * w);
    [link setStroke];
    [front_path stroke];

    NSBezierPath *j = [NSBezierPath bezierPath];
    [j moveToPoint:jr_pt(box, 0.474, 0.377)];
    [j lineToPoint:jr_pt(box, 0.635, 0.377)];
    [j lineToPoint:jr_pt(box, 0.635, 0.481)];
    [j curveToPoint:jr_pt(box, 0.522, 0.588)
      controlPoint1:jr_pt(box, 0.635, 0.551)
      controlPoint2:jr_pt(box, 0.586, 0.588)];
    [j curveToPoint:jr_pt(box, 0.428, 0.543)
      controlPoint1:jr_pt(box, 0.481, 0.588)
      controlPoint2:jr_pt(box, 0.446, 0.570)];
    j.lineWidth = MAX(1.4, 0.0586 * w);
    j.lineCapStyle = NSLineCapStyleRound;
    j.lineJoinStyle = NSLineJoinStyleRound;
    if (w >= 34) {
        NSGradient *gradient = [[NSGradient alloc] initWithStartingColor:jr_mint() endingColor:link];
        [gradient drawInBezierPath:j angle:-45];
    } else {
        [link setStroke];
        [j stroke];
    }

    if (with_stand && box.size.height > 26) {
        NSBezierPath *stand = [NSBezierPath bezierPath];
        [stand moveToPoint:jr_pt(box, 0.555, 0.664)];
        [stand lineToPoint:jr_pt(box, 0.555, 0.723)];
        [stand moveToPoint:jr_pt(box, 0.457, 0.740)];
        [stand lineToPoint:jr_pt(box, 0.652, 0.740)];
        stand.lineWidth = MAX(1.0, 0.040 * w);
        stand.lineCapStyle = NSLineCapStyleRound;
        [[ink colorWithAlphaComponent:0.30] setStroke];
        [stand stroke];
    }
}

// ---------------------------------------------------------------------------
// 视图类声明
// ---------------------------------------------------------------------------

/// 只画一个状态字形的小视图，侧栏、面板、横幅共用。
@interface JrpGlyphView : NSView
@property (nonatomic, assign) JrGlyph kind;
@property (nonatomic, strong) NSColor *color;
@end

@interface JrpBrandMarkView : NSView
@property (nonatomic, assign) BOOL withStand;
@end

/// 设备行：整行自定义绘制，名字、状态、机器短号各就其位。
@interface JrpDeviceRow : NSView
@property (nonatomic, copy) NSString *deviceId;
@property (nonatomic, copy) NSString *name;
@property (nonatomic, copy) NSString *meta;
@property (nonatomic, copy) NSString *status;
@property (nonatomic, assign) JrGlyph glyph;
@property (nonatomic, strong) NSColor *statusColor;
@property (nonatomic, assign) BOOL selected;
@property (nonatomic, assign) BOOL hovered;
@end

/// 列表容器：键盘上下选择、回车执行主要动作、直接输入就跳到搜索。
@interface JrpRosterHost : NSView
@end

/// 链路条：本机 → 信令 → 目标设备。
@interface JrpLinkStrip : NSView
@property (nonatomic, copy) NSString *localCode;
@property (nonatomic, copy) NSString *signalCode;
@property (nonatomic, copy) NSString *targetCode;
@property (nonatomic, copy) NSString *leg1;
@property (nonatomic, copy) NSString *leg2;
@property (nonatomic, assign) BOOL signal_online;
@property (nonatomic, assign) BOOL leg1_live;
@property (nonatomic, assign) BOOL leg2_live;
@end

/// 内嵌面板：一块带细描边的操作台。
@interface JrpPlate : NSView
@property (nonatomic, assign) BOOL filled;
@end

/// 会话横幅：只有真的在会话里才出现，和「服务器在线」明确区分。
@interface JrpBanner : NSView
@end

@interface JrpUi : NSObject <NSApplicationDelegate, NSWindowDelegate, NSTextFieldDelegate, NSSearchFieldDelegate>
@end

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

static jrp_ui_action g_action;
static void *g_action_ctx;

static NSWindow *g_win;
static NSVisualEffectView *g_sidebar;
static NSView *g_main;
static NSView *g_header;
static NSView *g_header_rule;
static NSView *g_activity;
static NSView *g_plate;
static NSView *g_actions;
static NSView *g_empty;
static NSView *g_banner;
static NSView *g_plate_rule;
static NSScrollView *g_device_scroll;
static JrpGlyphView *g_server_dot;
static JrpGlyphView *g_device_glyph;
static JrpGlyphView *g_banner_dot;
static JrpGlyphView *g_notice_dot;
static JrpBrandMarkView *g_plate_mark;
static JrpBrandMarkView *g_empty_mark;
static JrpLinkStrip *g_strip;
static NSSearchField *g_search;
static NSTextField *g_local_name;
static NSTextField *g_local_id;
static NSTextField *g_server_state;
static NSTextField *g_server_addr;
static NSTextField *g_devices_count;
static NSTextField *g_link_label;
static NSTextField *g_activity_label;
static NSTextField *g_device_name;
static NSTextField *g_device_code;
static NSTextField *g_device_status;
static NSTextField *g_device_desc;
static NSTextField *g_action_hint;
static NSTextField *g_notice;
static NSTextField *g_banner_title;
static NSTextField *g_banner_sub;
static NSTextField *g_footer;
static NSTextField *g_empty_title;
static NSTextField *g_empty_body;
static NSButton *g_auto;
static NSButton *g_end;
static NSButton *g_primary;
static NSButton *g_grant;
static NSButton *g_ungrant;
static NSButton *g_revoke;
static NSButton *g_wake;
static NSButton *g_logs;
static NSButton *g_permit;
static NSButton *g_empty_button;
static NSProgressIndicator *g_spinner;
static NSWindow *g_log_win;
static NSTextView *g_log_text;
static NSString *g_log_dir;
static char *g_packed;
static NSArray<NSDictionary<NSString *, id> *> *g_devices;
static NSMutableArray<NSString *> *g_order;
static NSString *g_selected_id;
static NSString *g_active_peer;
static NSString *g_signal_address;
static NSString *g_local_code;
static int g_signal_connected;
static int g_session_active;
static int g_controller;
static int g_can_view;
static int g_inviting;
static int g_autostart;
static int g_shown;
static int g_banner_shown;

static void render_device_list(void);
static void rebuild_rows(void);
static void update_detail(void);
static void update_banner(void);
static void layout_main(void);
static void layout_actions(void);
static NSDictionary<NSString *, id> *selected_device(void);
static NSArray<NSDictionary<NSString *, id> *> *visible_devices(void);
static void apply_selection(NSString *device_id, BOOL scroll);
static void primary_action(void);
static void fire(uint8_t kind, NSString *arg);

// ---------------------------------------------------------------------------
// 视图实现
// ---------------------------------------------------------------------------

@implementation JrpGlyphView
- (BOOL)isFlipped { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    jr_draw_glyph(self.bounds, self.kind, self.color ?: [NSColor secondaryLabelColor]);
}
@end

@implementation JrpBrandMarkView
- (BOOL)isFlipped { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    jr_draw_mark(self.bounds, self.withStand);
}
@end

@implementation JrpDeviceRow {
    NSTrackingArea *_tracking;
}
- (BOOL)isFlipped { return YES; }
- (NSFocusRingType)focusRingType { return NSFocusRingTypeNone; }

- (void)updateTrackingAreas {
    [super updateTrackingAreas];
    if (_tracking) {
        [self removeTrackingArea:_tracking];
    }
    _tracking = [[NSTrackingArea alloc] initWithRect:self.bounds
                                             options:NSTrackingMouseEnteredAndExited | NSTrackingActiveInKeyWindow | NSTrackingInVisibleRect
                                               owner:self
                                            userInfo:nil];
    [self addTrackingArea:_tracking];
}

- (void)mouseEntered:(NSEvent *)event {
    (void)event;
    self.hovered = YES;
}

- (void)mouseExited:(NSEvent *)event {
    (void)event;
    self.hovered = NO;
}

- (void)mouseDown:(NSEvent *)event {
    // 只改选中状态，不重建列表：正在处理事件的视图不能把自己销毁。
    // 点一下行就把键盘焦点交给列表：之后上下键、回车、直接输入都直接生效。
    [self.window makeFirstResponder:self.superview];
    apply_selection(self.deviceId, NO);
    if (event.clickCount >= 2) {
        primary_action();
    }
}

- (BOOL)accessibilityPerformPress {
    apply_selection(self.deviceId, NO);
    return YES;
}

- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    CGFloat h = self.bounds.size.height;
    NSRect body = NSInsetRect(self.bounds, 2, 1);
    NSBezierPath *plate = [NSBezierPath bezierPathWithRoundedRect:body xRadius:10 yRadius:10];
    if (self.selected) {
        [[jr_link() colorWithAlphaComponent:0.14] setFill];
        [plate fill];
        // 左侧链路条：选中的机器用品牌色标出来，比整行反白更克制。
        NSRect bar = NSMakeRect(4, 12, 3, h - 24);
        NSGradient *gradient = [[NSGradient alloc] initWithStartingColor:jr_mint() endingColor:jr_link()];
        [gradient drawInBezierPath:[NSBezierPath bezierPathWithRoundedRect:bar xRadius:1.5 yRadius:1.5] angle:-90];
    } else if (self.hovered) {
        [[[NSColor labelColor] colorWithAlphaComponent:0.055] setFill];
        [plate fill];
    }

    jr_draw_glyph(NSMakeRect(16, 14, 12, 12), self.glyph, self.statusColor);

    NSMutableParagraphStyle *truncating = [NSMutableParagraphStyle new];
    truncating.lineBreakMode = NSLineBreakByTruncatingTail;
    NSMutableParagraphStyle *right = [NSMutableParagraphStyle new];
    right.alignment = NSTextAlignmentRight;
    right.lineBreakMode = NSLineBreakByTruncatingTail;

    CGFloat status_width = 88;
    NSAttributedString *name_text = [[NSAttributedString alloc] initWithString:self.name ?: @"" attributes:@{
        NSFontAttributeName: jr_font(13, NSFontWeightSemibold),
        NSForegroundColorAttributeName: [NSColor labelColor],
        NSParagraphStyleAttributeName: truncating,
    }];
    [name_text drawInRect:NSMakeRect(36, 11, MAX(40, self.bounds.size.width - 36 - status_width - 14), 18)];

    NSAttributedString *status_text = [[NSAttributedString alloc] initWithString:self.status ?: @"" attributes:@{
        NSFontAttributeName: jr_font(11, NSFontWeightMedium),
        NSForegroundColorAttributeName: self.statusColor,
        NSParagraphStyleAttributeName: right,
    }];
    [status_text drawInRect:NSMakeRect(self.bounds.size.width - status_width - 14, 13, status_width, 16)];

    NSAttributedString *meta_text = [[NSAttributedString alloc] initWithString:self.meta ?: @"" attributes:@{
        NSFontAttributeName: jr_mono(10.5, NSFontWeightRegular),
        NSForegroundColorAttributeName: [NSColor secondaryLabelColor],
        NSParagraphStyleAttributeName: truncating,
    }];
    [meta_text drawInRect:NSMakeRect(36, 31, MAX(40, self.bounds.size.width - 50), 15)];
}

- (void)setSelected:(BOOL)selected {
    if (_selected == selected) {
        return;
    }
    _selected = selected;
    [self setNeedsDisplay:YES];
}

- (void)setHovered:(BOOL)hovered {
    if (_hovered == hovered) {
        return;
    }
    _hovered = hovered;
    [self setNeedsDisplay:YES];
}
@end

@implementation JrpRosterHost
- (BOOL)isFlipped { return YES; }
- (BOOL)acceptsFirstResponder { return YES; }
- (NSFocusRingType)focusRingType { return NSFocusRingTypeNone; }

- (void)keyDown:(NSEvent *)event {
    NSString *chars = event.charactersIgnoringModifiers ?: @"";
    unichar first = chars.length ? [chars characterAtIndex:0] : 0;
    if (first == NSUpArrowFunctionKey || first == NSDownArrowFunctionKey) {
        NSInteger index = -1;
        for (NSUInteger i = 0; i < g_order.count; i++) {
            if ([g_order[i] isEqualToString:g_selected_id]) {
                index = (NSInteger)i;
                break;
            }
        }
        if (index < 0) {
            index = (first == NSDownArrowFunctionKey) ? -1 : 0;
        }
        NSInteger next = (first == NSUpArrowFunctionKey) ? index - 1 : index + 1;
        if (next < 0) next = 0;
        if (next >= (NSInteger)g_order.count) next = (NSInteger)g_order.count - 1;
        if (next >= 0 && next < (NSInteger)g_order.count) {
            apply_selection(g_order[(NSUInteger)next], YES);
        }
        return;
    }
    if (first == NSCarriageReturnCharacter || first == NSEnterCharacter || first == NSNewlineCharacter) {
        primary_action();
        return;
    }
    if (first == 0x1B) {
        // Esc：清掉搜索词，回到完整舰队列表。响应者是这个列表本身，重建安全。
        if (g_search.stringValue.length) {
            g_search.stringValue = @"";
            render_device_list();
            update_detail();
        }
        return;
    }
    if (chars.length && first >= 0x20 && first != 0x7F && !(event.modifierFlags & NSEventModifierFlagCommand)) {
        [g_win makeFirstResponder:g_search];
        [g_search.currentEditor insertText:chars];
        return;
    }
    [super keyDown:event];
}
@end

@implementation JrpLinkStrip
- (BOOL)isFlipped { return YES; }

- (void)drawNode:(NSRect)box live:(BOOL)live {
    // 节点就是图标里的那块屏幕，缩到 34 × 24。
    NSColor *color = live ? jr_link() : [NSColor tertiaryLabelColor];
    NSBezierPath *path = [NSBezierPath bezierPathWithRoundedRect:box xRadius:4 yRadius:4];
    [[color colorWithAlphaComponent:live ? 0.16 : 0.10] setFill];
    [path fill];
    path.lineWidth = 1.4;
    [color setStroke];
    [path stroke];
    NSRect gauge = NSMakeRect(box.origin.x + 8, box.origin.y + box.size.height - 8, box.size.width - 16, 2.5);
    [[color colorWithAlphaComponent:live ? 0.9 : 0.4] setFill];
    [[NSBezierPath bezierPathWithRoundedRect:gauge xRadius:1.25 yRadius:1.25] fill];
}

- (void)drawLegFrom:(CGFloat)x1 to:(CGFloat)x2 y:(CGFloat)y label:(NSString *)label live:(BOOL)live {
    if (x2 - x1 < 12) {
        return;
    }
    NSColor *color = live ? jr_link() : [NSColor tertiaryLabelColor];
    CGFloat mid = (x1 + x2) / 2;
    CGFloat gap = 0;
    if (label.length) {
        NSMutableParagraphStyle *style = [NSMutableParagraphStyle new];
        style.alignment = NSTextAlignmentCenter;
        style.lineBreakMode = NSLineBreakByTruncatingTail;
        NSAttributedString *text = [[NSAttributedString alloc] initWithString:label attributes:@{
            NSFontAttributeName: jr_font(10, NSFontWeightMedium),
            NSForegroundColorAttributeName: live ? color : [NSColor secondaryLabelColor],
            NSParagraphStyleAttributeName: style,
        }];
        CGFloat width = MIN(text.size.width + 4, x2 - x1 - 8);
        [text drawInRect:NSMakeRect(mid - width / 2, y - 7, width, 14)];
        gap = width / 2 + 3;
    }
    // 线分两段，中间给文字让位，这样不用给文字垫底色。
    [[color colorWithAlphaComponent:live ? 0.85 : 0.35] setStroke];
    NSArray<NSArray<NSNumber *> *> *segments = @[@[@(x1), @(mid - gap)], @[@(mid + gap), @(x2)]];
    for (NSArray<NSNumber *> *segment in segments) {
        CGFloat a = segment[0].doubleValue;
        CGFloat b = segment[1].doubleValue;
        if (b - a < 2) {
            continue;
        }
        NSBezierPath *line = [NSBezierPath bezierPath];
        [line moveToPoint:NSMakePoint(a, y)];
        [line lineToPoint:NSMakePoint(b, y)];
        line.lineWidth = 1.4;
        [line stroke];
    }
}

- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    CGFloat w = self.bounds.size.width;
    CGFloat node_w = 34;
    CGFloat node_h = 24;
    CGFloat gap = 12;
    NSRect left = NSMakeRect(0, 0, node_w, node_h);
    NSRect middle = NSMakeRect(w / 2 - node_w / 2, 0, node_w, node_h);
    NSRect right = NSMakeRect(w - node_w, 0, node_w, node_h);
    CGFloat line_y = node_h / 2;

    [self drawLegFrom:NSMaxX(left) + gap to:NSMinX(middle) - gap y:line_y label:self.leg1 live:self.leg1_live];
    [self drawLegFrom:NSMaxX(middle) + gap to:NSMinX(right) - gap y:line_y label:self.leg2 live:self.leg2_live];
    [self drawNode:left live:YES];
    [self drawNode:middle live:self.signal_online];
    [self drawNode:right live:self.leg2_live];

    NSArray<NSString *> *titles = @[@"本机", @"信令服务器", @"目标设备"];
    NSArray<NSString *> *values = @[self.localCode ?: @"", self.signalCode ?: @"", self.targetCode.length ? self.targetCode : @"—"];
    NSArray<NSValue *> *boxes = @[[NSValue valueWithRect:left], [NSValue valueWithRect:middle], [NSValue valueWithRect:right]];
    for (NSUInteger i = 0; i < 3; i++) {
        CGFloat center = NSMidX(boxes[i].rectValue);
        CGFloat width = MIN(w / 3.0 - 4, 220);
        NSMutableParagraphStyle *style = [NSMutableParagraphStyle new];
        style.alignment = NSTextAlignmentCenter;
        style.lineBreakMode = NSLineBreakByTruncatingMiddle;
        NSAttributedString *title = [[NSAttributedString alloc] initWithString:titles[i] attributes:@{
            NSFontAttributeName: jr_font(10.5, NSFontWeightMedium),
            NSForegroundColorAttributeName: [NSColor secondaryLabelColor],
            NSParagraphStyleAttributeName: style,
        }];
        [title drawInRect:NSMakeRect(center - width / 2, node_h + 4, width, 14)];
        BOOL offline = (i == 1 && !self.signal_online);
        NSAttributedString *value = [[NSAttributedString alloc] initWithString:values[i] attributes:@{
            NSFontAttributeName: jr_mono(10.5, NSFontWeightRegular),
            NSForegroundColorAttributeName: offline ? [NSColor systemOrangeColor] : [NSColor labelColor],
            NSParagraphStyleAttributeName: style,
        }];
        [value drawInRect:NSMakeRect(center - width / 2, node_h + 19, width, 15)];
    }
}
@end

@implementation JrpPlate
- (BOOL)isFlipped { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    if (!self.filled) {
        return;
    }
    NSRect body = NSInsetRect(self.bounds, 0.5, 0.5);
    NSBezierPath *path = [NSBezierPath bezierPathWithRoundedRect:body xRadius:12 yRadius:12];
    [jr_surface() setFill];
    [path fill];
    path.lineWidth = 1;
    [jr_hairline() setStroke];
    [path stroke];
}
@end

@implementation JrpBanner
- (BOOL)isFlipped { return YES; }
- (void)drawRect:(NSRect)dirty {
    (void)dirty;
    NSRect body = NSInsetRect(self.bounds, 0.5, 0.5);
    NSBezierPath *path = [NSBezierPath bezierPathWithRoundedRect:body xRadius:10 yRadius:10];
    [[jr_link() colorWithAlphaComponent:0.12] setFill];
    [path fill];
    path.lineWidth = 1;
    [[jr_link() colorWithAlphaComponent:0.35] setStroke];
    [path stroke];
    NSRect bar = NSMakeRect(1, 10, 3, self.bounds.size.height - 20);
    [jr_link() setFill];
    [[NSBezierPath bezierPathWithRoundedRect:bar xRadius:1.5 yRadius:1.5] fill];
}
@end

// ---------------------------------------------------------------------------
// 动作
// ---------------------------------------------------------------------------

static void fire(uint8_t kind, NSString *arg) {
    if (!g_action) {
        return;
    }
    const char *text = arg ? [arg UTF8String] : "";
    g_action(kind, text ? text : "", g_action_ctx);
}

@implementation JrpUi
- (void)invite:(id)sender {
    (void)sender;
    primary_action();
}
- (void)grant:(id)sender {
    (void)sender;
    if (!g_selected_id) return;
    NSDictionary<NSString *, id> *device = selected_device();
    NSAlert *alert = [NSAlert new];
    alert.messageText = @"授予无人值守权限？";
    alert.informativeText = [NSString stringWithFormat:@"允许「%@」在授权有效期内连接并控制这台 Mac，无须每次在本机点允许。关掉这条授权或撤销设备后立刻失效。", device[@"name"] ?: g_selected_id];
    [alert addButtonWithTitle:@"授予权限"];
    [alert addButtonWithTitle:@"取消"];
    if ([alert runModal] == NSAlertFirstButtonReturn) fire(JR_ACT_GRANT, g_selected_id);
}
- (void)ungrant:(id)sender {
    (void)sender;
    if (g_selected_id) fire(JR_ACT_UNGRANT, g_selected_id);
}
- (void)revoke:(id)sender {
    (void)sender;
    if (!g_selected_id) return;
    NSDictionary<NSString *, id> *device = selected_device();
    NSAlert *alert = [NSAlert new];
    alert.messageText = [NSString stringWithFormat:@"把「%@」移出舰队？", device[@"name"] ?: g_selected_id];
    alert.informativeText = @"撤销后这台设备不能再连接本机，已经发出的无人值守授权同时作废。";
    [alert addButtonWithTitle:@"撤销设备"];
    [alert addButtonWithTitle:@"取消"];
    if ([alert runModal] == NSAlertFirstButtonReturn) fire(JR_ACT_REVOKE, g_selected_id);
}
- (void)wake:(id)sender {
    (void)sender;
    if (g_selected_id) fire(JR_ACT_WAKE, g_selected_id);
}
- (void)searchChanged:(id)sender {
    (void)sender;
    render_device_list();
    update_detail();
}
- (void)focusSearch:(id)sender {
    (void)sender;
    [g_win makeFirstResponder:g_search];
}
- (void)focusRoster:(id)sender {
    (void)sender;
    if (g_device_scroll.documentView) {
        [g_win makeFirstResponder:g_device_scroll.documentView];
    }
}
- (void)toggleAuto:(id)sender {
    NSButton *box = (NSButton *)sender;
    fire(JR_ACT_AUTOSTART, box.state == NSControlStateValueOn ? @"1" : @"0");
}
- (void)hangup:(id)sender {
    (void)sender;
    fire(JR_ACT_END, @"");
}
- (void)rescan:(id)sender {
    (void)sender;
    fire(JR_ACT_RESCAN, @"");
}
- (void)view:(id)sender {
    (void)sender;
    fire(JR_ACT_VIEW, @"");
}
- (void)permit:(id)sender {
    (void)sender;
    fire(JR_ACT_PERMIT, @"");
}
- (void)showLogs:(id)sender {
    (void)sender;
    fire(JR_ACT_LOGS, @"");
}
- (void)refreshLogs:(id)sender {
    (void)sender;
    jrp_ui_show_logs(g_log_dir.UTF8String);
}
- (void)openLogFolder:(id)sender {
    (void)sender;
    if (g_log_dir) {
        [[NSWorkspace sharedWorkspace] openURL:[NSURL fileURLWithPath:g_log_dir isDirectory:YES]];
    }
}
- (BOOL)control:(NSControl *)control textView:(NSTextView *)textView doCommandBySelector:(SEL)commandSelector {
    if (control == g_search && [NSStringFromSelector(commandSelector) isEqualToString:@"cancelOperation:"]) {
        // 搜索框里按 Esc：清掉搜索词并立刻回到完整舰队列表，不停留在旧过滤上。
        if (g_search.stringValue.length) {
            g_search.stringValue = @"";
            render_device_list();
            update_detail();
        }
        (void)textView;
        return YES;
    }
    return NO;
}
- (void)quit:(id)sender {
    (void)sender;
    fire(JR_ACT_QUIT, @"");
}
- (void)reopen:(id)sender {
    (void)sender;
    [g_win makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    g_shown = 1;
}
- (void)windowWillClose:(NSNotification *)notification {
    (void)notification;
    g_shown = 0;
    fire(JR_ACT_QUIT, @"");
}
- (void)windowDidResize:(NSNotification *)notification {
    (void)notification;
    layout_main();
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    (void)sender;
    return NSTerminateNow;
}
@end

static JrpUi *g_target;

// ---------------------------------------------------------------------------
// 设备列表
// ---------------------------------------------------------------------------

static NSDictionary<NSString *, id> *selected_device(void) {
    for (NSDictionary<NSString *, id> *device in g_devices) {
        if ([device[@"id"] isEqualToString:g_selected_id]) return device;
    }
    return nil;
}

static NSArray<NSDictionary<NSString *, id> *> *visible_devices(void) {
    NSString *query = g_search.stringValue.lowercaseString ?: @"";
    NSMutableArray<NSDictionary<NSString *, id> *> *visible = [NSMutableArray array];
    for (NSDictionary<NSString *, id> *device in g_devices) {
        if (query.length == 0 || [[device[@"name"] lowercaseString] containsString:query] ||
            [[device[@"id"] lowercaseString] containsString:query]) {
            [visible addObject:device];
        }
    }
    return visible;
}

static NSString *jr_device_meta(NSDictionary<NSString *, id> *device) {
    unsigned int flags = [device[@"flags"] unsignedIntValue];
    NSString *net = device[@"net"] ?: @"";
    NSMutableArray<NSString *> *parts = [NSMutableArray array];
    if (net.length) {
        [parts addObject:(flags & JR_ROW_LAN) ? [NSString stringWithFormat:@"%@直连", net] : net];
    }
    [parts addObject:jr_format_code(device[@"id"])];
    if (flags & JR_ROW_HAS_GRANT) {
        [parts addObject:@"无人值守"];
    }
    return [parts componentsJoinedByString:@"   ·   "];
}

static void render_device_list(void) {
    if (!g_device_scroll || !g_device_scroll.documentView) return;
    JrpRosterHost *host = (JrpRosterHost *)g_device_scroll.documentView;
    NSArray<NSDictionary<NSString *, id> *> *visible = visible_devices();

    NSMutableArray<NSDictionary<NSString *, id> *> *open_group = [NSMutableArray array];
    NSMutableArray<NSDictionary<NSString *, id> *> *other_group = [NSMutableArray array];
    for (NSDictionary<NSString *, id> *device in visible) {
        unsigned int flags = [device[@"flags"] unsignedIntValue];
        if ((flags & JR_ROW_CAN_INVITE) || (flags & JR_ROW_ONLINE)) {
            [open_group addObject:device];
        } else {
            [other_group addObject:device];
        }
    }

    for (NSView *child in [host.subviews copy]) {
        [child removeFromSuperview];
    }
    g_order = [NSMutableArray array];

    CGFloat width = MAX(196, g_device_scroll.contentSize.width);
    CGFloat row_height = 58;
    CGFloat heading_height = 26;
    __block CGFloat y = 6;

    void (^place)(NSString *, NSArray<NSDictionary<NSString *, id> *> *) =
        ^(NSString *title, NSArray<NSDictionary<NSString *, id> *> *group) {
        if (group.count == 0) {
            return;
        }
        NSTextField *heading = jr_section_label(title);
        heading.frame = NSMakeRect(16, y + 6, width - 26, 16);
        [host addSubview:heading];
        y += heading_height;
        for (NSDictionary<NSString *, id> *device in group) {
            unsigned int flags = [device[@"flags"] unsignedIntValue];
            NSString *status = device[@"status"] ?: @"";
            JrpDeviceRow *row = [[JrpDeviceRow alloc] initWithFrame:NSMakeRect(4, y, width - 8, row_height)];
            row.deviceId = device[@"id"];
            row.name = device[@"name"];
            row.meta = jr_device_meta(device);
            row.status = status;
            row.glyph = jr_glyph_for(status, flags);
            row.statusColor = jr_status_color(status, flags);
            row.selected = [device[@"id"] isEqualToString:g_selected_id];
            row.toolTip = [NSString stringWithFormat:@"%@ · %@", device[@"name"], status];
            row.accessibilityRole = NSAccessibilityRowRole;
            row.accessibilityLabel = [NSString stringWithFormat:@"%@，%@，%@", device[@"name"], status, row.meta];
            [host addSubview:row];
            [g_order addObject:device[@"id"]];
            y += row_height;
        }
    };
    place(@"可连接", open_group);
    place(@"其他", other_group);

    if (visible.count == 0) {
        NSString *text = g_devices.count
            ? [NSString stringWithFormat:@"没有匹配「%@」的设备", g_search.stringValue]
            : @"还没有其他设备";
        NSTextField *empty = jr_label(text, 12, NSFontWeightRegular, [NSColor secondaryLabelColor]);
        empty.frame = NSMakeRect(16, 18, width - 32, 18);
        [host addSubview:empty];
    }
    host.frame = NSMakeRect(0, 0, width, MAX(g_device_scroll.contentSize.height, y + 12));
    jr_set_section_text(g_devices_count, g_devices.count
        ? [NSString stringWithFormat:@"%lu 台设备", (unsigned long)g_devices.count]
        : @"暂无设备");
}

/// 只改选中状态和明细，不重建列表：可以在设备行自己的事件处理里安全调用。
static void apply_selection(NSString *device_id, BOOL scroll) {
    if (!device_id) {
        return;
    }
    if (![device_id isEqualToString:g_selected_id]) {
        g_selected_id = device_id;
        g_inviting = 0;
    }
    NSView *host = g_device_scroll.documentView;
    for (NSView *child in host.subviews) {
        if (![child isKindOfClass:[JrpDeviceRow class]]) {
            continue;
        }
        JrpDeviceRow *row = (JrpDeviceRow *)child;
        BOOL selected = [row.deviceId isEqualToString:g_selected_id];
        row.selected = selected;
        if (selected && scroll) {
            [host scrollRectToVisible:NSInsetRect(row.frame, 0, -8)];
        }
    }
    update_detail();
}

static void primary_action(void) {
    if (g_session_active) {
        if (g_controller && g_can_view && [g_selected_id isEqualToString:g_active_peer]) {
            fire(JR_ACT_VIEW, @"");
        }
        return;
    }
    if (!g_selected_id || g_inviting) {
        return;
    }
    NSDictionary<NSString *, id> *device = selected_device();
    unsigned int flags = device ? [device[@"flags"] unsignedIntValue] : 0;
    if (!device || !(flags & JR_ROW_CAN_INVITE) || !g_signal_connected) {
        return;
    }
    g_inviting = 1;
    update_detail();
    fire(JR_ACT_INVITE, g_selected_id);
}

static void rebuild_rows(void) {
    NSMutableArray<NSDictionary<NSString *, id> *> *rows = [NSMutableArray array];
    if (g_packed) {
        NSArray<NSString *> *lines = [@(g_packed) componentsSeparatedByString:@"\n"];
        for (NSUInteger i = 1; i < lines.count; i++) {
            NSArray<NSString *> *parts = [lines[i] componentsSeparatedByString:@"\t"];
            if (parts.count < 5 || [parts[0] length] == 0) continue;
            [rows addObject:@{@"id": parts[0], @"name": parts[1], @"status": parts[2],
                @"net": parts[3], @"flags": @([parts[4] integerValue])}];
        }
    }
    g_devices = rows;
    if (!selected_device()) {
        g_selected_id = rows.count ? rows[0][@"id"] : nil;
    }
    render_device_list();
    update_detail();
}

// ---------------------------------------------------------------------------
// 明细与会话横幅
// ---------------------------------------------------------------------------

static void update_banner(void) {
    if (!g_banner) return;
    BOOL visible = g_session_active != 0;
    g_banner.hidden = !visible;
    g_banner_title.stringValue = visible
        ? (g_controller ? [NSString stringWithFormat:@"正在控制 %@", g_active_peer.length ? g_active_peer : @"对端"]
                        : [NSString stringWithFormat:@"本机正被 %@ 控制", g_active_peer.length ? g_active_peer : @"对端"])
        : @"";
    g_banner_sub.stringValue = visible
        ? (g_controller ? @"端到端加密会话进行中" : @"本机屏幕与输入正在被对端使用")
        : @"";
    g_banner_dot.color = jr_link();
    g_banner_dot.kind = g_controller ? JrGlyphBusy : JrGlyphLan;
    [g_banner_dot setNeedsDisplay:YES];
    if (visible && !g_banner_shown) {
        // 会话建立是个重要状态变化：短暂淡入，其余状态一律即时切换。
        g_banner.alphaValue = 0;
        [NSAnimationContext runAnimationGroup:^(NSAnimationContext *ctx) {
            ctx.duration = 0.18;
            g_banner.animator.alphaValue = 1;
        } completionHandler:nil];
    } else if (!visible) {
        g_banner.alphaValue = 1;
    }
    g_banner_shown = visible ? 1 : 0;
}

static void update_detail(void) {
    if (!g_device_name) return;
    NSDictionary<NSString *, id> *device = selected_device();
    unsigned int flags = device ? [device[@"flags"] unsignedIntValue] : 0;
    BOOL selected = device != nil;
    BOOL active_here = g_session_active && [g_selected_id isEqualToString:g_active_peer];
    BOOL can_invite = selected && (flags & JR_ROW_CAN_INVITE) && g_signal_connected && !g_session_active && !g_inviting;
    NSString *status = selected ? (device[@"status"] ?: @"") : @"";

    if (selected) {
        g_device_name.stringValue = device[@"name"];
        g_device_code.stringValue = jr_format_code(device[@"id"]);
        g_device_status.stringValue = status;
        g_device_status.textColor = jr_status_color(status, flags);
        g_device_glyph.kind = jr_glyph_for(status, flags);
        g_device_glyph.color = jr_status_color(status, flags);
        [g_device_glyph setNeedsDisplay:YES];
    } else {
        g_device_name.stringValue = g_devices.count ? @"选择一台设备" : @"还没有其他设备";
        g_device_code.stringValue = @"";
        g_device_status.stringValue = @"";
    }

    NSString *host = g_signal_address.length ? [g_signal_address componentsSeparatedByString:@":"].firstObject : @"";
    BOOL target_live = selected && (flags & JR_ROW_CAN_INVITE);
    g_strip.localCode = g_local_code ?: @"";
    g_strip.signalCode = host.length ? host : @"—";
    g_strip.targetCode = selected ? jr_format_code(device[@"id"]) : @"—";
    g_strip.signal_online = g_signal_connected != 0;
    g_strip.leg1 = g_signal_connected ? @"已连接" : @"重连中";
    g_strip.leg2 = active_here ? @"加密会话"
        : (!selected ? @"" : (target_live ? ((flags & JR_ROW_LAN) ? @"局域网直连" : @"经服务器") : status));
    g_strip.leg1_live = g_signal_connected != 0;
    g_strip.leg2_live = active_here || target_live;
    [g_strip setNeedsDisplay:YES];

    if (g_session_active) {
        g_device_desc.stringValue = g_controller
            ? @"安全会话已建立。画面就绪后可以观看，随时可以从上方断开。"
            : @"安全会话已建立，本机正在接受远程控制。";
        g_primary.title = (active_here && g_controller) ? (g_can_view ? @"观看画面" : @"等待画面") : @"会话进行中";
        g_primary.enabled = active_here && g_controller && g_can_view;
    } else {
        if (!selected) {
            g_device_desc.stringValue = @"";
        } else if (g_inviting) {
            g_device_desc.stringValue = @"邀请已发出，等待对方在本机确认。";
        } else if (!g_signal_connected) {
            g_device_desc.stringValue = @"信令服务器正在重连。恢复之后可以再次发送邀请。";
        } else if (flags & JR_ROW_CAN_INVITE) {
            g_device_desc.stringValue = @"设备在线，可以发送邀请。对方允许后自动打开观看窗口。";
        } else {
            g_device_desc.stringValue = [NSString stringWithFormat:@"当前状态：%@。状态恢复后可以发起邀请。", status];
        }
        g_primary.title = g_inviting ? @"邀请中…" : @"邀请控制";
        g_primary.enabled = can_invite;
    }
    g_primary.bezelColor = (g_primary.enabled && !g_session_active) ? jr_link() : nil;
    g_spinner.hidden = !g_inviting;
    if (g_inviting) {
        [g_spinner startAnimation:nil];
    } else {
        [g_spinner stopAnimation:nil];
    }

    g_action_hint.stringValue = !selected ? @""
        : (g_session_active
            ? (g_controller ? @"会话期间可以随时断开；断开后本机屏幕与输入恢复。" : @"本机正在被控制，本地输入同时有效。")
            : (g_inviting ? @"已发出邀请，等待对方在本机点允许。"
                : ((flags & JR_ROW_HAS_GRANT) ? @"已授权无人值守：授权有效期内无须对方再次确认。"
                    : ((flags & JR_ROW_CAN_GRANT) ? @"未授权无人值守：每次连接都要对方在本机点一次允许。" : @""))));

    g_grant.hidden = !selected || g_session_active || !(flags & JR_ROW_CAN_GRANT);
    g_ungrant.hidden = !selected || g_session_active || !(flags & JR_ROW_HAS_GRANT);
    g_revoke.hidden = !selected || g_session_active || !(flags & JR_ROW_CAN_REVOKE);
    g_wake.hidden = !selected || g_session_active || (flags & (JR_ROW_ONLINE | JR_ROW_REVOKED));
    g_wake.enabled = g_signal_connected && !(flags & JR_ROW_REVOKED);
    g_device_glyph.hidden = !selected;
    update_banner();
    layout_main();
}

// ---------------------------------------------------------------------------
// 布局
// ---------------------------------------------------------------------------

static CGFloat const JR_SIDEBAR_W = 282;
static CGFloat const JR_PAD = 28;
static CGFloat const JR_HEADER_H = 56;
static CGFloat const JR_PLATE_H = 200;
static CGFloat const JR_BANNER_H = 48;

static void layout_actions(void) {
    // 最小窗口下主区只有 561pt：从右往左排，保证“唤醒”不会被裁掉。
    CGFloat cw = g_actions ? MAX(320, g_actions.bounds.size.width) : 640;
    CGFloat total = 524;
    CGFloat x = MAX(0, cw - total);
    g_primary.frame = NSMakeRect(x, 0, 140, 34);
    g_spinner.frame = NSMakeRect(x + 140 - 22, 10, 14, 14);
    g_grant.frame = NSMakeRect(x + 146, 0, 120, 34);
    g_ungrant.frame = NSMakeRect(x + 272, 0, 88, 34);
    g_revoke.frame = NSMakeRect(x + 366, 0, 88, 34);
    g_wake.frame = NSMakeRect(x + 460, 0, 64, 34);
    g_action_hint.frame = NSMakeRect(0, 42, cw, 15);
}

static void layout_main(void) {
    if (!g_main) return;
    CGFloat w = g_main.bounds.size.width;
    CGFloat h = g_main.bounds.size.height;
    CGFloat cw = MAX(320, w - JR_PAD * 2);
    CGFloat y = JR_HEADER_H + 18;

    g_header.frame = jr_top(h, 0, 0, w, JR_HEADER_H);
    g_header_rule.frame = jr_top(h, JR_HEADER_H, 0, w, 1);
    CGFloat local_width = MAX(120, w - JR_PAD - 176 - 20 - (JR_PAD + 34));
    g_local_name.frame = jr_top(h, 13, JR_PAD + 32, local_width, 18);
    g_local_id.frame = jr_top(h, 32, JR_PAD + 33, local_width, 15);
    g_permit.frame = jr_top(h, 14, w - JR_PAD - 96, 96, 30);
    g_logs.frame = jr_top(h, 14, w - JR_PAD - 96 - 8 - 72, 72, 30);

    if (g_session_active) {
        g_banner.frame = jr_top(h, y, JR_PAD, cw, JR_BANNER_H);
        y += JR_BANNER_H + 18;
    }
    CGFloat end_x = MAX(0, cw - 16 - 96);
    g_end.frame = NSMakeRect(end_x, 10, 96, 28);
    g_banner_title.frame = NSMakeRect(42, 11, MAX(80, end_x - 58), 18);
    g_banner_sub.frame = NSMakeRect(42, 29, MAX(80, end_x - 58), 15);

    BOOL has_device = selected_device() != nil;
    if (has_device) {
        g_link_label.frame = jr_top(h, y, JR_PAD, cw, 16);
        y += 16 + 8;
        g_plate.frame = jr_top(h, y, JR_PAD, cw, JR_PLATE_H);
        y += JR_PLATE_H + 14;
        g_actions.frame = jr_top(h, y, JR_PAD, cw, 58);
        layout_actions();
        y += 58 + 16;
    } else {
        g_empty.frame = jr_top(h, y, JR_PAD, cw, 320);
        y += 320 + 4;
    }
    g_link_label.hidden = !has_device;
    g_plate.hidden = !has_device;
    g_actions.hidden = !has_device;
    g_empty.hidden = has_device;

    g_activity_label.frame = jr_top(h, y, JR_PAD, cw, 16);
    y += 16 + 8;
    g_activity.frame = jr_top(h, y, JR_PAD, cw, 56);

    // 空状态内部：整块居中，读起来像一页引导而不是一张卡片。
    CGFloat eh = g_empty.bounds.size.height;
    CGFloat ew = g_empty.bounds.size.width;
    g_empty_mark.frame = NSMakeRect(ew / 2 - 30, eh - 84, 60, 60);
    g_empty_title.frame = NSMakeRect(0, eh - 136, ew, 24);
    g_empty_body.frame = NSMakeRect(MAX(0, ew / 2 - 210), eh - 180, MIN(ew, 420), 44);
    g_empty_button.frame = NSMakeRect(ew / 2 - 66, eh - 236, 132, 32);

    // 面板内部
    CGFloat pw = g_plate.bounds.size.width;
    CGFloat right_w = 158;
    g_plate_mark.frame = NSMakeRect(24, 20, 44, 44);
    g_device_name.frame = NSMakeRect(86, 16, MAX(80, pw - 86 - 24 - right_w - 16), 28);
    g_device_code.frame = NSMakeRect(pw - 24 - right_w, 22, right_w, 16);
    g_device_code.alignment = NSTextAlignmentRight;
    g_device_glyph.frame = NSMakeRect(86, 52, 12, 12);
    g_device_status.frame = NSMakeRect(102, 49, MAX(80, pw - 128), 17);
    g_device_desc.frame = NSMakeRect(26, 82, MAX(120, pw - 52), 32);
    g_plate_rule.frame = NSMakeRect(26, 120, MAX(120, pw - 52), 1);
    g_strip.frame = NSMakeRect(26, 128, MAX(160, pw - 52), 58);

    // 动态面板
    g_notice_dot.frame = NSMakeRect(20, 21, 12, 12);
    g_notice.frame = NSMakeRect(44, 10, MAX(120, cw - 64), 36);
    g_action_hint.frame = NSMakeRect(0, 42, MAX(120, cw), 15);

    // 底部
    g_auto.frame = NSMakeRect(JR_PAD, 16, 140, 22);
    g_footer.frame = NSMakeRect(w - JR_PAD - 260, 19, 260, 15);
}

// ---------------------------------------------------------------------------
// 建窗
// ---------------------------------------------------------------------------

static void build_window(void) {
    if (g_win) return;
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
    g_win = [[NSWindow alloc] initWithContentRect:NSMakeRect(120, 130, 1120, 720)
        styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable
        backing:NSBackingStoreBuffered defer:NO];
    g_win.title = @"Jaryn Remote";
    g_win.minSize = NSMakeSize(900, 600);
    g_win.releasedWhenClosed = NO;
    g_target = [JrpUi new];
    g_win.delegate = g_target;
    NSApp.delegate = g_target;

    NSMenu *menubar = [NSMenu new];
    NSMenuItem *app_item = [NSMenuItem new];
    [menubar addItem:app_item];
    NSMenu *app_menu = [NSMenu new];
    [app_menu addItemWithTitle:@"显示设备窗口" action:@selector(reopen:) keyEquivalent:@"0"].target = g_target;
    [app_menu addItem:[NSMenuItem separatorItem]];
    [app_menu addItemWithTitle:@"退出 Jaryn Remote" action:@selector(quit:) keyEquivalent:@"q"].target = g_target;
    app_item.submenu = app_menu;

    NSMenuItem *device_item = [NSMenuItem new];
    [menubar addItem:device_item];
    NSMenu *device_menu = [NSMenu new];
    device_menu.title = @"设备";
    [device_menu addItemWithTitle:@"搜索设备" action:@selector(focusSearch:) keyEquivalent:@"f"].target = g_target;
    [device_menu addItemWithTitle:@"回到设备列表" action:@selector(focusRoster:) keyEquivalent:@"j"].target = g_target;
    [device_menu addItem:[NSMenuItem separatorItem]];
    [device_menu addItemWithTitle:@"重新扫描" action:@selector(rescan:) keyEquivalent:@"r"].target = g_target;
    [device_menu addItemWithTitle:@"查看本机日志" action:@selector(showLogs:) keyEquivalent:@"l"].target = g_target;
    [device_menu addItem:[NSMenuItem separatorItem]];
    [device_menu addItemWithTitle:@"断开会话" action:@selector(hangup:) keyEquivalent:@"."].target = g_target;
    device_item.submenu = device_menu;
    [NSApp setMainMenu:menubar];
    [NSApp finishLaunching];

    NSView *content = g_win.contentView;
    CGFloat h = content.bounds.size.height;
    CGFloat w = content.bounds.size.width;
    NSSplitView *split = [[NSSplitView alloc] initWithFrame:content.bounds];
    split.vertical = YES;
    split.dividerStyle = NSSplitViewDividerStyleThin;
    split.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [content addSubview:split];

    g_sidebar = [[NSVisualEffectView alloc] initWithFrame:NSMakeRect(0, 0, JR_SIDEBAR_W, h)];
    g_sidebar.material = NSVisualEffectMaterialSidebar;
    g_sidebar.blendingMode = NSVisualEffectBlendingModeBehindWindow;
    g_sidebar.state = NSVisualEffectStateActive;
    [split addSubview:g_sidebar];
    g_main = [[NSView alloc] initWithFrame:NSMakeRect(JR_SIDEBAR_W + 1, 0, w - JR_SIDEBAR_W - 1, h)];
    g_main.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    [split addSubview:g_main];
    [split setPosition:JR_SIDEBAR_W ofDividerAtIndex:0];

    // ---- 侧栏：品牌 → 搜索 → 设备列表 → 服务器状态 ----
    JrpBrandMarkView *mark = [[JrpBrandMarkView alloc] initWithFrame:NSMakeRect(18, h - 52, 30, 30)];
    mark.autoresizingMask = NSViewMinYMargin;
    [g_sidebar addSubview:mark];
    NSTextField *brand = jr_label(@"Jaryn Remote", 15, NSFontWeightSemibold, nil);
    brand.frame = NSMakeRect(56, h - 38, 200, 19);
    brand.autoresizingMask = NSViewMinYMargin;
    [g_sidebar addSubview:brand];
    NSTextField *tagline = jr_label(@"私有链路工作台", 10.5, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    tagline.frame = NSMakeRect(56, h - 55, 200, 15);
    tagline.autoresizingMask = NSViewMinYMargin;
    [g_sidebar addSubview:tagline];
    NSView *brand_rule = jr_rule();
    brand_rule.frame = NSMakeRect(14, h - 68, JR_SIDEBAR_W - 28, 1);
    brand_rule.autoresizingMask = NSViewMinYMargin;
    [g_sidebar addSubview:brand_rule];

    g_search = [[NSSearchField alloc] initWithFrame:NSMakeRect(14, h - 106, JR_SIDEBAR_W - 28, 30)];
    g_search.placeholderString = @"搜索名称或设备短号";
    g_search.sendsSearchStringImmediately = YES;
    g_search.target = g_target;
    g_search.action = @selector(searchChanged:);
    g_search.delegate = g_target;
    g_search.autoresizingMask = NSViewWidthSizable | NSViewMinYMargin;
    [g_sidebar addSubview:g_search];

    g_devices_count = jr_section_label(@"");
    g_devices_count.frame = NSMakeRect(18, h - 130, JR_SIDEBAR_W - 110, 15);
    g_devices_count.autoresizingMask = NSViewMinYMargin;
    [g_sidebar addSubview:g_devices_count];
    NSButton *rescan = [NSButton buttonWithTitle:@"刷新" target:g_target action:@selector(rescan:)];
    rescan.frame = NSMakeRect(JR_SIDEBAR_W - 72, h - 134, 56, 24);
    rescan.bezelStyle = NSBezelStyleRounded;
    rescan.controlSize = NSControlSizeSmall;
    rescan.font = jr_font(11, NSFontWeightMedium);
    rescan.autoresizingMask = NSViewMinXMargin | NSViewMinYMargin;
    rescan.toolTip = @"重新扫描舰队（⌘R）";
    [g_sidebar addSubview:rescan];

    g_device_scroll = [[NSScrollView alloc] initWithFrame:NSMakeRect(6, 74, JR_SIDEBAR_W - 12, h - 216)];
    g_device_scroll.hasVerticalScroller = YES;
    g_device_scroll.autohidesScrollers = YES;
    g_device_scroll.drawsBackground = NO;
    g_device_scroll.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    JrpRosterHost *host = [[JrpRosterHost alloc] initWithFrame:NSMakeRect(0, 0, JR_SIDEBAR_W - 12, 200)];
    g_device_scroll.documentView = host;
    [g_sidebar addSubview:g_device_scroll];

    NSView *server_rule = jr_rule();
    server_rule.frame = NSMakeRect(14, 60, JR_SIDEBAR_W - 28, 1);
    server_rule.autoresizingMask = NSViewWidthSizable;
    [g_sidebar addSubview:server_rule];
    g_server_dot = [[JrpGlyphView alloc] initWithFrame:NSMakeRect(18, 34, 12, 12)];
    g_server_dot.kind = JrGlyphOffline;
    g_server_dot.color = [NSColor systemOrangeColor];
    g_server_dot.autoresizingMask = NSViewMaxYMargin;
    [g_sidebar addSubview:g_server_dot];
    g_server_state = jr_label(@"服务器重连中", 11.5, NSFontWeightSemibold, [NSColor systemOrangeColor]);
    g_server_state.frame = NSMakeRect(36, 31, JR_SIDEBAR_W - 50, 17);
    g_server_state.autoresizingMask = NSViewWidthSizable | NSViewMaxYMargin;
    [g_sidebar addSubview:g_server_state];
    g_server_addr = jr_mono_label(@"", 10.5, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    g_server_addr.frame = NSMakeRect(36, 16, JR_SIDEBAR_W - 50, 15);
    g_server_addr.autoresizingMask = NSViewWidthSizable | NSViewMaxYMargin;
    [g_sidebar addSubview:g_server_addr];

    // ---- 主区顶栏：这台机器是谁 + 固定入口 ----
    g_header = [[NSView alloc] init];
    [g_main addSubview:g_header];
    JrpBrandMarkView *chip_mark = [[JrpBrandMarkView alloc] initWithFrame:NSMakeRect(JR_PAD, 16, 24, 24)];
    [g_header addSubview:chip_mark];
    g_local_name = jr_label(@"正在读取…", 13.5, NSFontWeightSemibold, nil);
    [g_header addSubview:g_local_name];
    g_local_id = jr_mono_label(@"", 10.5, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    [g_header addSubview:g_local_id];
    g_permit = [NSButton buttonWithTitle:@"系统权限" target:g_target action:@selector(permit:)];
    g_permit.bezelStyle = NSBezelStyleRounded;
    g_permit.font = jr_font(12, NSFontWeightMedium);
    g_permit.toolTip = @"申请屏幕录制与辅助功能权限";
    [g_header addSubview:g_permit];
    g_logs = [NSButton buttonWithTitle:@"日志" target:g_target action:@selector(showLogs:)];
    g_logs.bezelStyle = NSBezelStyleRounded;
    g_logs.font = jr_font(12, NSFontWeightMedium);
    g_logs.toolTip = @"查看本机日志（⌘L）";
    [g_header addSubview:g_logs];
    g_header_rule = jr_rule();
    [g_main addSubview:g_header_rule];

    // ---- 会话横幅 ----
    g_banner = [[JrpBanner alloc] init];
    [g_main addSubview:g_banner];
    g_banner_dot = [[JrpGlyphView alloc] initWithFrame:NSMakeRect(18, 16, 14, 14)];
    g_banner_dot.kind = JrGlyphBusy;
    g_banner_dot.color = jr_link();
    [g_banner addSubview:g_banner_dot];
    g_banner_title = jr_label(@"", 12.5, NSFontWeightSemibold, nil);
    [g_banner addSubview:g_banner_title];
    g_banner_sub = jr_label(@"", 11, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    [g_banner addSubview:g_banner_sub];
    g_end = [NSButton buttonWithTitle:@"断开会话" target:g_target action:@selector(hangup:)];
    g_end.bezelStyle = NSBezelStyleRounded;
    g_end.font = jr_font(12, NSFontWeightMedium);
    g_end.toolTip = @"结束当前会话（⌘.）";
    [g_banner addSubview:g_end];

    // ---- 链路面板 ----
    g_link_label = jr_section_label(@"设备链路");
    [g_main addSubview:g_link_label];
    g_plate = [[JrpPlate alloc] init];
    ((JrpPlate *)g_plate).filled = YES;
    [g_main addSubview:g_plate];
    g_plate_mark = [[JrpBrandMarkView alloc] initWithFrame:NSMakeRect(24, 20, 44, 44)];
    g_plate_mark.withStand = YES;
    [g_plate addSubview:g_plate_mark];
    g_device_name = jr_label(@"选择一台设备", 23, NSFontWeightSemibold, nil);
    [g_plate addSubview:g_device_name];
    g_device_code = jr_mono_label(@"", 11, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    [g_plate addSubview:g_device_code];
    g_device_glyph = [[JrpGlyphView alloc] initWithFrame:NSMakeRect(86, 52, 12, 12)];
    g_device_glyph.color = [NSColor secondaryLabelColor];
    [g_plate addSubview:g_device_glyph];
    g_device_status = jr_label(@"", 11.5, NSFontWeightMedium, [NSColor secondaryLabelColor]);
    [g_plate addSubview:g_device_status];
    g_device_desc = jr_label(@"", 12.5, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    g_device_desc.lineBreakMode = NSLineBreakByWordWrapping;
    [g_plate addSubview:g_device_desc];
    g_plate_rule = jr_rule();
    [g_plate addSubview:g_plate_rule];
    g_strip = [[JrpLinkStrip alloc] init];
    [g_plate addSubview:g_strip];

    // ---- 主要动作 ----
    g_actions = [[NSView alloc] init];
    [g_main addSubview:g_actions];
    g_primary = [NSButton buttonWithTitle:@"邀请控制" target:g_target action:@selector(invite:)];
    g_primary.bezelStyle = NSBezelStyleRounded;
    g_primary.font = jr_font(13, NSFontWeightSemibold);
    g_primary.toolTip = @"邀请对方允许本次控制（在设备行上双击同样可以）";
    [g_actions addSubview:g_primary];
    g_spinner = [[NSProgressIndicator alloc] init];
    g_spinner.style = NSProgressIndicatorStyleSpinning;
    g_spinner.controlSize = NSControlSizeSmall;
    g_spinner.displayedWhenStopped = NO;
    g_spinner.hidden = YES;
    [g_actions addSubview:g_spinner];
    g_grant = [NSButton buttonWithTitle:@"授权无人值守" target:g_target action:@selector(grant:)];
    g_grant.bezelStyle = NSBezelStyleRounded;
    g_grant.font = jr_font(12.5, NSFontWeightMedium);
    g_grant.toolTip = @"生成一条由本机签名、可以随时撤回的长期授权";
    [g_actions addSubview:g_grant];
    g_ungrant = [NSButton buttonWithTitle:@"撤回授权" target:g_target action:@selector(ungrant:)];
    g_ungrant.bezelStyle = NSBezelStyleRounded;
    g_ungrant.font = jr_font(12.5, NSFontWeightMedium);
    [g_actions addSubview:g_ungrant];
    g_revoke = [NSButton buttonWithTitle:@"撤销设备" target:g_target action:@selector(revoke:)];
    g_revoke.bezelStyle = NSBezelStyleRounded;
    g_revoke.attributedTitle = [[NSAttributedString alloc] initWithString:@"撤销设备" attributes:@{
        NSFontAttributeName: jr_font(12.5, NSFontWeightMedium),
        NSForegroundColorAttributeName: [NSColor systemRedColor],
    }];
    [g_actions addSubview:g_revoke];
    g_wake = [NSButton buttonWithTitle:@"唤醒" target:g_target action:@selector(wake:)];
    g_wake.bezelStyle = NSBezelStyleRounded;
    g_wake.font = jr_font(12.5, NSFontWeightMedium);
    g_wake.toolTip = @"通过同网常开节点代发魔包；公网中继不能叫醒关机的主机";
    [g_actions addSubview:g_wake];
    g_action_hint = jr_label(@"", 11, NSFontWeightRegular, [NSColor tertiaryLabelColor]);
    [g_actions addSubview:g_action_hint];

    // ---- 空状态：舰队里还没有其他设备时的引导 ----
    g_empty = [[NSView alloc] init];
    [g_main addSubview:g_empty];
    g_empty_mark = [[JrpBrandMarkView alloc] initWithFrame:NSMakeRect(0, 0, 60, 60)];
    g_empty_mark.withStand = YES;
    [g_empty addSubview:g_empty_mark];
    g_empty_title = jr_label(@"还没有其他设备", 17, NSFontWeightSemibold, nil);
    g_empty_title.alignment = NSTextAlignmentCenter;
    [g_empty addSubview:g_empty_title];
    g_empty_body = jr_label(@"左侧现在只有这台 Mac。配对成功后，舰队里的其他设备会出现在这里。",
                            12.5, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    g_empty_body.alignment = NSTextAlignmentCenter;
    g_empty_body.lineBreakMode = NSLineBreakByWordWrapping;
    [g_empty addSubview:g_empty_body];
    g_empty_button = [NSButton buttonWithTitle:@"重新扫描" target:g_target action:@selector(rescan:)];
    g_empty_button.bezelStyle = NSBezelStyleRounded;
    g_empty_button.font = jr_font(12.5, NSFontWeightMedium);
    [g_empty addSubview:g_empty_button];

    // ---- 最近动态 ----
    g_activity_label = jr_section_label(@"最近动态");
    [g_main addSubview:g_activity_label];
    g_activity = [[JrpPlate alloc] init];
    ((JrpPlate *)g_activity).filled = YES;
    [g_main addSubview:g_activity];
    g_notice_dot = [[JrpGlyphView alloc] initWithFrame:NSMakeRect(20, 21, 12, 12)];
    g_notice_dot.kind = JrGlyphOnline;
    g_notice_dot.color = [NSColor tertiaryLabelColor];
    [g_activity addSubview:g_notice_dot];
    g_notice = jr_label(@"等待操作 · 邀请进度和错误会显示在这里", 12, NSFontWeightRegular, [NSColor secondaryLabelColor]);
    g_notice.lineBreakMode = NSLineBreakByWordWrapping;
    [g_activity addSubview:g_notice];

    // ---- 底部：显式设置 + 归属 ----
    g_auto = [NSButton checkboxWithTitle:@"开机自启" target:g_target action:@selector(toggleAuto:)];
    g_auto.font = jr_font(12, NSFontWeightRegular);
    g_auto.toolTip = @"登录时自动运行本机节点（默认关闭）";
    [g_main addSubview:g_auto];
    g_footer = jr_label(@"自建信令与中继 · 端到端加密", 10.5, NSFontWeightRegular, [NSColor tertiaryLabelColor]);
    g_footer.alignment = NSTextAlignmentRight;
    [g_main addSubview:g_footer];

    update_detail();
}

// ---------------------------------------------------------------------------
// 本机日志
// ---------------------------------------------------------------------------

void jrp_ui_show_logs(const char *dir) {
    if (![NSThread isMainThread]) {
        return;
    }
    if (dir) {
        g_log_dir = [NSString stringWithUTF8String:dir];
    }
    if (!g_log_dir) {
        return;
    }
    if (!g_log_win) {
        NSRect frame = NSMakeRect(180, 160, 940, 640);
        g_log_win = [[NSWindow alloc] initWithContentRect:frame
                                               styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable
                                                 backing:NSBackingStoreBuffered defer:NO];
        g_log_win.title = @"本机日志";
        g_log_win.releasedWhenClosed = NO;
        NSView *content = g_log_win.contentView;
        CGFloat h = content.bounds.size.height;
        CGFloat w = content.bounds.size.width;

        NSTextField *title = jr_label(@"本机日志", 15, NSFontWeightSemibold, nil);
        title.frame = NSMakeRect(18, h - 36, 200, 19);
        title.autoresizingMask = NSViewMinYMargin;
        [content addSubview:title];
        NSTextField *hint = jr_label(@"本机保留的完整记录；新条目请点刷新", 11, NSFontWeightRegular, [NSColor secondaryLabelColor]);
        hint.frame = NSMakeRect(18, h - 54, 380, 15);
        hint.autoresizingMask = NSViewMinYMargin;
        [content addSubview:hint];

        NSButton *folder = [NSButton buttonWithTitle:@"打开日志文件夹" target:g_target action:@selector(openLogFolder:)];
        folder.frame = NSMakeRect(w - 18 - 148, h - 42, 148, 28);
        folder.bezelStyle = NSBezelStyleRounded;
        folder.font = jr_font(12, NSFontWeightMedium);
        folder.autoresizingMask = NSViewMinXMargin | NSViewMinYMargin;
        [content addSubview:folder];
        NSButton *refresh = [NSButton buttonWithTitle:@"刷新" target:g_target action:@selector(refreshLogs:)];
        refresh.frame = NSMakeRect(w - 18 - 148 - 8 - 72, h - 42, 72, 28);
        refresh.bezelStyle = NSBezelStyleRounded;
        refresh.font = jr_font(12, NSFontWeightMedium);
        refresh.autoresizingMask = NSViewMinXMargin | NSViewMinYMargin;
        [content addSubview:refresh];

        NSView *rule = jr_rule();
        rule.frame = NSMakeRect(0, h - 68, w, 1);
        rule.autoresizingMask = NSViewWidthSizable | NSViewMinYMargin;
        [content addSubview:rule];

        NSScrollView *scroll = [[NSScrollView alloc] initWithFrame:NSMakeRect(0, 28, w, h - 96)];
        scroll.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        scroll.hasVerticalScroller = YES;
        scroll.drawsBackground = NO;
        g_log_text = [[NSTextView alloc] initWithFrame:scroll.contentView.bounds];
        g_log_text.editable = NO;
        g_log_text.selectable = YES;
        g_log_text.drawsBackground = NO;
        g_log_text.font = jr_mono(11, NSFontWeightRegular);
        g_log_text.textColor = [NSColor labelColor];
        g_log_text.verticallyResizable = YES;
        g_log_text.maxSize = NSMakeSize(FLT_MAX, FLT_MAX);
        g_log_text.textContainerInset = NSMakeSize(18, 12);
        g_log_text.textContainer.containerSize = NSMakeSize(scroll.contentSize.width, FLT_MAX);
        g_log_text.textContainer.widthTracksTextView = YES;
        g_log_text.autoresizingMask = NSViewWidthSizable;
        scroll.documentView = g_log_text;
        [content addSubview:scroll];

        NSTextField *path = jr_mono_label(g_log_dir, 10, NSFontWeightRegular, [NSColor tertiaryLabelColor]);
        path.frame = NSMakeRect(18, 7, w - 36, 14);
        path.autoresizingMask = NSViewWidthSizable;
        [content addSubview:path];
    }

    NSMutableString *all = [NSMutableString string];
    for (NSInteger n = 4; n >= 0; n--) {
        NSString *name = n == 0 ? @"client.log" : [NSString stringWithFormat:@"client.log.%ld", (long)n];
        NSString *path = [g_log_dir stringByAppendingPathComponent:name];
        if (![[NSFileManager defaultManager] fileExistsAtPath:path]) {
            continue;
        }
        NSError *error = nil;
        NSString *body = [NSString stringWithContentsOfFile:path encoding:NSUTF8StringEncoding error:&error];
        [all appendFormat:@"===== %@ =====\n", name];
        [all appendString:body ?: [NSString stringWithFormat:@"读取失败：%@\n", error.localizedDescription ?: @"未知错误"]];
        [all appendString:@"\n"];
    }
    if (all.length == 0) {
        [all appendFormat:@"尚无本机日志。日志目录：%@", g_log_dir];
    }
    g_log_text.string = all;
    [g_log_text scrollRangeToVisible:NSMakeRange(g_log_text.string.length, 0)];
    [g_log_win makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
}

// ---------------------------------------------------------------------------
// 来自 Rust 的更新
// ---------------------------------------------------------------------------

void jrp_ui_action_cb(jrp_ui_action cb, void *ctx) {
    g_action = cb;
    g_action_ctx = ctx;
}

void jrp_ui_set_rows(const char *packed) {
    if (![NSThread isMainThread]) {
        return;
    }
    build_window();
    free(g_packed);
    g_packed = dup_c(packed);
    rebuild_rows();
}

void jrp_ui_set_autostart(int on) {
    if (![NSThread isMainThread] || !g_auto) {
        return;
    }
    g_autostart = on ? 1 : 0;
    g_auto.state = g_autostart ? NSControlStateValueOn : NSControlStateValueOff;
}

void jrp_ui_set_can_view(int on) {
    if (![NSThread isMainThread]) return;
    if (g_can_view != (on ? 1 : 0)) {
        g_can_view = on ? 1 : 0;
        update_detail();
    }
}

void jrp_ui_set_overview(const char *name, const char *short_id, const char *signal,
                         int connected, int session_active, const char *session_peer, int controller) {
    if (![NSThread isMainThread]) return;
    build_window();
    BOOL was_active = g_session_active;
    g_signal_connected = connected != 0;
    g_session_active = session_active != 0;
    g_controller = controller != 0;
    g_active_peer = [NSString stringWithUTF8String:session_peer ?: ""];
    g_signal_address = [NSString stringWithUTF8String:signal ? signal : ""];
    g_local_name.stringValue = [NSString stringWithUTF8String:name ? name : ""];
    g_local_code = jr_format_code([NSString stringWithUTF8String:short_id ? short_id : ""]);
    g_local_id.stringValue = g_local_code.length ? [NSString stringWithFormat:@"本机短号  %@", g_local_code] : @"";
    g_server_state.stringValue = g_signal_connected ? @"服务器已连接" : @"服务器重连中";
    g_server_state.textColor = g_signal_connected ? [NSColor systemGreenColor] : [NSColor systemOrangeColor];
    g_server_addr.stringValue = g_signal_address;
    g_server_dot.kind = g_signal_connected ? JrGlyphLan : JrGlyphOffline;
    g_server_dot.color = g_signal_connected ? [NSColor systemGreenColor] : [NSColor systemOrangeColor];
    [g_server_dot setNeedsDisplay:YES];
    if (g_session_active && !was_active && g_active_peer.length) {
        g_selected_id = g_active_peer;
        g_inviting = 0;
        render_device_list();
    }
    update_detail();
}

void jrp_ui_error(const char *title, const char *message) {
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
    [NSApp activateIgnoringOtherApps:YES];
    NSAlert *alert = [NSAlert new];
    alert.messageText = title ? @(title) : @"Jaryn Remote";
    alert.informativeText = message ? @(message) : @"";
    [alert addButtonWithTitle:@"确定"];
    [alert runModal];
}

static BOOL valid_hex(NSString *text) {
    if (text.length != 64) {
        return NO;
    }
    NSCharacterSet *non_hex = [[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdefABCDEF"] invertedSet];
    return [text rangeOfCharacterFromSet:non_hex].location == NSNotFound;
}

int jrp_ui_setup(const char *default_signal, const char *default_name,
                 char *signal, size_t signal_cap, char *name, size_t name_cap,
                 char *token, size_t token_cap, char *pin, size_t pin_cap) {
    if (![NSThread isMainThread]) {
        return 0;
    }
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
    [NSApp activateIgnoringOtherApps:YES];

    NSView *form = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, 470, 148)];
    NSArray<NSString *> *labels = @[@"信令 IP:端口", @"设备名称", @"fleet-token", @"cert-fingerprint"];
    NSMutableArray<NSTextField *> *fields = [NSMutableArray array];
    for (NSUInteger i = 0; i < labels.count; i++) {
        CGFloat y = 112 - (CGFloat)i * 37;
        NSTextField *field_label = [NSTextField labelWithString:labels[i]];
        field_label.frame = NSMakeRect(0, y + 3, 130, 22);
        field_label.font = jr_font(12, NSFontWeightMedium);
        [form addSubview:field_label];
        NSTextField *field = i == 2 ? [NSSecureTextField new] : [NSTextField new];
        field.frame = NSMakeRect(137, y, 330, 26);
        field.font = jr_mono(12, NSFontWeightRegular);
        [form addSubview:field];
        [fields addObject:field];
    }
    fields[0].stringValue = default_signal ? @(default_signal) : @"127.0.0.1:47210";
    fields[1].stringValue = default_name ? @(default_name) : @"Mac";
    while (YES) {
        NSAlert *alert = [NSAlert new];
        alert.messageText = @"连接 Jaryn Remote";
        alert.informativeText = @"首次使用请输入信令地址和证书指纹；若信令开启了封闭配对模式（--private），fleet-token 可留空。授权信息只保存在本机。";
        alert.accessoryView = form;
        [alert addButtonWithTitle:@"连接"];
        [alert addButtonWithTitle:@"取消"];
        if ([alert runModal] != NSAlertFirstButtonReturn) {
            return 0;
        }
        NSString *address = [fields[0].stringValue stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
        NSString *device = [fields[1].stringValue stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
        NSString *fleet = [fields[2].stringValue stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
        if (fleet.length == 0) {
            fleet = @"0000000000000000000000000000000000000000000000000000000000000000";
        }
        NSString *fingerprint = [fields[3].stringValue stringByTrimmingCharactersInSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]];
        if (address.length == 0 || device.length == 0 || !valid_hex(fleet) || !valid_hex(fingerprint)) {
            jrp_ui_error("信息不完整", "请填写信令地址、设备名称与 64 位十六进制证书指纹（普通模式还需填写 64 位 fleet-token）。");
            continue;
        }
        if (![address getCString:signal maxLength:signal_cap encoding:NSUTF8StringEncoding] ||
            ![device getCString:name maxLength:name_cap encoding:NSUTF8StringEncoding] ||
            ![fleet getCString:token maxLength:token_cap encoding:NSUTF8StringEncoding] ||
            ![fingerprint getCString:pin maxLength:pin_cap encoding:NSUTF8StringEncoding]) {
            jrp_ui_error("内容过长", "信令地址和设备名称请控制在 127 字节以内。");
            continue;
        }
        return 1;
    }
}

void jrp_ui_set_notice(const char *text) {
    if (![NSThread isMainThread]) {
        return;
    }
    build_window();
    NSString *message = [NSString stringWithUTF8String:text ? text : ""];
    g_notice.stringValue = message.length ? message : @"等待操作 · 邀请进度和错误会显示在这里";
    // 只根据文案前缀和关键词区分提示语气，不改变任何业务判断。
    BOOL warning = [message containsString:@"失败"] || [message containsString:@"无法"]
        || [message containsString:@"拒绝"] || [message containsString:@"错误"] || [message containsString:@"超时"];
    BOOL success = !warning && message.length > 0 && [message hasPrefix:@"已"];
    g_notice_dot.color = warning ? [NSColor systemOrangeColor]
        : (success ? [NSColor systemGreenColor] : [NSColor tertiaryLabelColor]);
    g_notice_dot.kind = warning ? JrGlyphNeeds : JrGlyphOnline;
    [g_notice_dot setNeedsDisplay:YES];
    g_notice.textColor = warning ? [NSColor systemOrangeColor]
        : (message.length ? [NSColor labelColor] : [NSColor secondaryLabelColor]);
    if (![message hasPrefix:@"正在邀请"]) {
        g_inviting = 0;
    }
    update_detail();
}

void jrp_ui_show(int on) {
    if (![NSThread isMainThread]) {
        return;
    }
    build_window();
    if (on) {
        [g_win makeKeyAndOrderFront:nil];
        [NSApp activateIgnoringOtherApps:YES];
        g_shown = 1;
    } else {
        [g_win orderOut:nil];
        g_shown = 0;
    }
}

int jrp_ui_is_open(void) {
    return g_win && g_shown;
}

/// 阻塞式确认框，只在主线程调。用户自己点，程序不代答。
int jrp_ui_ask(const char *title, const char *body) {
    if (![NSThread isMainThread]) {
        return 0;
    }
    [NSApplication sharedApplication];
    NSAlert *alert = [NSAlert new];
    alert.messageText = title ? @(title) : @"";
    alert.informativeText = body ? @(body) : @"";
    [alert addButtonWithTitle:@"允许"];
    [alert addButtonWithTitle:@"拒绝"];
    NSInteger answer = [alert runModal];
    return answer == NSAlertFirstButtonReturn ? 1 : 0;
}

void jrp_ui_pump(void) {
    if (![NSThread isMainThread] || !NSApp) {
        return;
    }
    NSDate *until = [NSDate dateWithTimeIntervalSinceNow:0.002];
    NSEvent *event = nil;
    while ((event = [NSApp nextEventMatchingMask:NSEventMaskAny untilDate:until inMode:NSDefaultRunLoopMode dequeue:YES])) {
        [NSApp sendEvent:event];
        until = [NSDate date];
    }
}
