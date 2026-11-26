#import "ToolbarNaming.h"

#import <AppKit/AppKit.h>
#import <objc/runtime.h>

/// The name given to tab view controllers' toolbars, or nil.
static NSString *_Nullable tabToolbarName;

@implementation NSToolbar (TesseraTestsNaming)

/// Exchanged with `-initWithIdentifier:`, so a call of it runs AppKit's.
- (instancetype)initTesseraTestsWithIdentifier:(NSToolbarIdentifier)identifier {
    NSString *name = tabToolbarName;
    if (name != nil && [identifier hasPrefix:@"NSTabViewControllerToolbarUIProvider("]) {
        identifier = name;
    }
    return [self initTesseraTestsWithIdentifier:identifier];
}

@end

void TesseraTestsNameTabViewControllerToolbars(NSString *_Nullable name) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        method_exchangeImplementations(
            class_getInstanceMethod(NSToolbar.class, @selector(initWithIdentifier:)),
            class_getInstanceMethod(NSToolbar.class, @selector(initTesseraTestsWithIdentifier:)));
    });
    tabToolbarName = [name copy];
}
