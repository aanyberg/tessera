// Names a tab view controller's toolbar as a test chooses (#742).

#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

/// From now on, every toolbar AppKit makes for a tab view controller (one
/// named `NSTabViewControllerToolbarUIProvider(0x...)`, after the address
/// of an object of AppKit's own) is named `name` instead, or, for nil, as
/// AppKit names it. A test thus gives the Settings window's toolbar a name
/// whose item list it set beforehand, as a window whose object lands at the
/// address of an earlier, freed one takes that one's list. The first call
/// exchanges `-[NSToolbar initWithIdentifier:]` for the rest of the process;
/// later calls, nil included, change only the name. Main thread only.
void TesseraTestsNameTabViewControllerToolbars(NSString *_Nullable name);

NS_ASSUME_NONNULL_END
