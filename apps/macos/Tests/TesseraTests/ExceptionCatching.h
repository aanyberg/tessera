// The test target's bridging header: what Swift cannot do itself.

#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

/// Runs `block` and returns the Objective-C exception that ended it, or
/// nil. Swift has no `@catch`, and an exception left to XCTest ends the
/// test while an async test's task runs on into the next (PaneTests'
/// divider drags ended by an exception in AppKit, #379).
NSException *_Nullable TesseraTestsCatchingException(NS_NOESCAPE void (^block)(void));

NS_ASSUME_NONNULL_END
