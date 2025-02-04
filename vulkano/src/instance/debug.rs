// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or https://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Debug callback called by intermediate layers or by the driver.
//!
//! When working on an application, it is recommended to register a debug callback. For example if
//! you enable the validation layers provided by the official Vulkan SDK, they will warn you about
//! invalid API usages or performance problems by calling this callback. The callback can also
//! be called by the driver or by whatever intermediate layer is activated.
//!
//! Note that the vulkano library can also emit messages to warn you about performance issues.
//! TODO: ^ that's not the case yet, need to choose whether we keep this idea
//!
//! # Example
//!
//! ```
//! # use vulkano::instance::Instance;
//! # use std::sync::Arc;
//! # let instance: Arc<Instance> = return;
//! use vulkano::instance::debug::DebugCallback;
//!
//! let _callback = DebugCallback::errors_and_warnings(&instance, |msg| {
//!     println!("Debug callback: {:?}", msg.description);
//! }).ok();
//! ```
//!
//! The type of `msg` in the callback is [`Message`].
//!
//! Note that you must keep the `_callback` object alive for as long as you want your callback to
//! be callable. If you don't store the return value of `DebugCallback`'s constructor in a
//! variable, it will be immediately destroyed and your callback will not work.
//!

use crate::check_errors;
use crate::instance::Instance;
use crate::Error;
use crate::VulkanObject;
use std::error;
use std::ffi::CStr;
use std::fmt;
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::panic;
use std::ptr;
use std::sync::Arc;

/// Registration of a callback called by validation layers.
///
/// The callback can be called as long as this object is alive.
#[must_use = "The DebugCallback object must be kept alive for as long as you want your callback \
              to be called"]
pub struct DebugCallback {
    instance: Arc<Instance>,
    debug_report_callback: ash::vk::DebugUtilsMessengerEXT,
    user_callback: Box<Box<dyn Fn(&Message) + Send>>,
}

impl DebugCallback {
    /// Initializes a debug callback.
    ///
    /// Panics generated by calling `user_callback` are ignored.
    pub fn new<F>(
        instance: &Arc<Instance>,
        severity: MessageSeverity,
        ty: MessageType,
        user_callback: F,
    ) -> Result<DebugCallback, DebugCallbackCreationError>
    where
        F: Fn(&Message) + 'static + Send + panic::RefUnwindSafe,
    {
        if !instance.enabled_extensions().ext_debug_utils {
            return Err(DebugCallbackCreationError::MissingExtension);
        }

        // Note that we need to double-box the callback, because a `*const Fn()` is a fat pointer
        // that can't be cast to a `*const c_void`.
        let user_callback = Box::new(Box::new(user_callback) as Box<_>);

        unsafe extern "system" fn callback(
            severity: ash::vk::DebugUtilsMessageSeverityFlagsEXT,
            ty: ash::vk::DebugUtilsMessageTypeFlagsEXT,
            callback_data: *const ash::vk::DebugUtilsMessengerCallbackDataEXT,
            user_data: *mut c_void,
        ) -> ash::vk::Bool32 {
            let user_callback = user_data as *mut Box<dyn Fn()> as *const _;
            let user_callback: &Box<dyn Fn(&Message)> = &*user_callback;

            let layer_prefix = (*callback_data)
                .p_message_id_name
                .as_ref()
                .map(|msg_id_name| {
                    CStr::from_ptr(msg_id_name)
                        .to_str()
                        .expect("debug callback message not utf-8")
                });

            let description = CStr::from_ptr((*callback_data).p_message)
                .to_str()
                .expect("debug callback message not utf-8");

            let message = Message {
                severity: MessageSeverity {
                    information: !(severity & ash::vk::DebugUtilsMessageSeverityFlagsEXT::INFO)
                        .is_empty(),
                    warning: !(severity & ash::vk::DebugUtilsMessageSeverityFlagsEXT::WARNING)
                        .is_empty(),
                    error: !(severity & ash::vk::DebugUtilsMessageSeverityFlagsEXT::ERROR)
                        .is_empty(),
                    verbose: !(severity & ash::vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE)
                        .is_empty(),
                },
                ty: MessageType {
                    general: !(ty & ash::vk::DebugUtilsMessageTypeFlagsEXT::GENERAL).is_empty(),
                    validation: !(ty & ash::vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION)
                        .is_empty(),
                    performance: !(ty & ash::vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE)
                        .is_empty(),
                },
                layer_prefix,
                description,
            };

            // Since we box the closure, the type system doesn't detect that the `UnwindSafe`
            // bound is enforced. Therefore we enforce it manually.
            let _ = panic::catch_unwind(panic::AssertUnwindSafe(move || {
                user_callback(&message);
            }));

            ash::vk::FALSE
        }

        let severity = {
            let mut flags = ash::vk::DebugUtilsMessageSeverityFlagsEXT::empty();
            if severity.information {
                flags |= ash::vk::DebugUtilsMessageSeverityFlagsEXT::INFO;
            }
            if severity.warning {
                flags |= ash::vk::DebugUtilsMessageSeverityFlagsEXT::WARNING;
            }
            if severity.error {
                flags |= ash::vk::DebugUtilsMessageSeverityFlagsEXT::ERROR;
            }
            if severity.verbose {
                flags |= ash::vk::DebugUtilsMessageSeverityFlagsEXT::VERBOSE;
            }
            flags
        };

        let ty = {
            let mut flags = ash::vk::DebugUtilsMessageTypeFlagsEXT::empty();
            if ty.general {
                flags |= ash::vk::DebugUtilsMessageTypeFlagsEXT::GENERAL;
            }
            if ty.validation {
                flags |= ash::vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION;
            }
            if ty.performance {
                flags |= ash::vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE;
            }
            flags
        };

        let infos = ash::vk::DebugUtilsMessengerCreateInfoEXT {
            flags: ash::vk::DebugUtilsMessengerCreateFlagsEXT::empty(),
            message_severity: severity,
            message_type: ty,
            pfn_user_callback: Some(callback),
            p_user_data: &*user_callback as &Box<_> as *const Box<_> as *const c_void as *mut _,
            ..Default::default()
        };

        let fns = instance.fns();

        let debug_report_callback = unsafe {
            let mut output = MaybeUninit::uninit();
            check_errors(fns.ext_debug_utils.create_debug_utils_messenger_ext(
                instance.internal_object(),
                &infos,
                ptr::null(),
                output.as_mut_ptr(),
            ))?;
            output.assume_init()
        };

        Ok(DebugCallback {
            instance: instance.clone(),
            debug_report_callback,
            user_callback,
        })
    }

    /// Initializes a debug callback with errors and warnings.
    ///
    /// Shortcut for `new(instance, MessageTypes::errors_and_warnings(), user_callback)`.
    #[inline]
    pub fn errors_and_warnings<F>(
        instance: &Arc<Instance>,
        user_callback: F,
    ) -> Result<DebugCallback, DebugCallbackCreationError>
    where
        F: Fn(&Message) + Send + 'static + panic::RefUnwindSafe,
    {
        DebugCallback::new(
            instance,
            MessageSeverity::errors_and_warnings(),
            MessageType::general(),
            user_callback,
        )
    }
}

impl Drop for DebugCallback {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let fns = self.instance.fns();
            fns.ext_debug_utils.destroy_debug_utils_messenger_ext(
                self.instance.internal_object(),
                self.debug_report_callback,
                ptr::null(),
            );
        }
    }
}

/// A message received by the callback.
pub struct Message<'a> {
    /// Severity of message.
    pub severity: MessageSeverity,
    /// Type of message,
    pub ty: MessageType,
    /// Prefix of the layer that reported this message or `None` if unknown.
    pub layer_prefix: Option<&'a str>,
    /// Description of the message.
    pub description: &'a str,
}

/// Severity of message.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageSeverity {
    /// An error that may cause undefined results, including an application crash.
    pub error: bool,
    /// An unexpected use.
    pub warning: bool,
    /// An informational message that may be handy when debugging an application.
    pub information: bool,
    /// Diagnostic information from the loader and layers.
    pub verbose: bool,
}

impl MessageSeverity {
    /// Builds a `MessageSeverity` with all fields set to `false` expect `error`.
    #[inline]
    pub const fn errors() -> MessageSeverity {
        MessageSeverity {
            error: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false` expect `warning`.
    #[inline]
    pub const fn warnings() -> MessageSeverity {
        MessageSeverity {
            warning: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false` expect `information`.
    #[inline]
    pub const fn information() -> MessageSeverity {
        MessageSeverity {
            information: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false` expect `verbose`.
    #[inline]
    pub const fn verbose() -> MessageSeverity {
        MessageSeverity {
            verbose: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false` expect `error`, `warning`
    /// and `performance_warning`.
    #[inline]
    pub const fn errors_and_warnings() -> MessageSeverity {
        MessageSeverity {
            error: true,
            warning: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false`.
    #[inline]
    pub const fn none() -> MessageSeverity {
        MessageSeverity {
            error: false,
            warning: false,
            information: false,
            verbose: false,
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `true`.
    #[inline]
    pub const fn all() -> MessageSeverity {
        MessageSeverity {
            error: true,
            warning: true,
            information: true,
            verbose: true,
        }
    }
}

impl std::ops::BitOr for MessageSeverity {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        MessageSeverity {
            error: self.error | rhs.error,
            warning: self.warning | rhs.warning,
            information: self.information | rhs.information,
            verbose: self.verbose | rhs.verbose,
        }
    }
}

/// Type of message.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageType {
    /// Specifies that some general event has occurred.
    pub general: bool,
    /// Specifies that something has occurred during validation against the vulkan specification
    pub validation: bool,
    /// Specifies a potentially non-optimal use of Vulkan
    pub performance: bool,
}

impl MessageType {
    /// Builds a `MessageType` with general field set to `true`.
    #[inline]
    pub const fn general() -> MessageType {
        MessageType {
            general: true,
            validation: false,
            performance: false,
        }
    }

    /// Builds a `MessageType` with validation field set to `true`.
    #[inline]
    pub const fn validation() -> MessageType {
        MessageType {
            general: false,
            validation: true,
            performance: false,
        }
    }

    /// Builds a `MessageType` with performance field set to `true`.
    #[inline]
    pub const fn performance() -> MessageType {
        MessageType {
            general: false,
            validation: false,
            performance: true,
        }
    }

    /// Builds a `MessageType` with all fields set to `true`.
    #[inline]
    pub const fn all() -> MessageType {
        MessageType {
            general: true,
            validation: true,
            performance: true,
        }
    }

    /// Builds a `MessageType` with all fields set to `false`.
    #[inline]
    pub const fn none() -> MessageType {
        MessageType {
            general: false,
            validation: false,
            performance: false,
        }
    }
}

impl std::ops::BitOr for MessageType {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        MessageType {
            general: self.general | rhs.general,
            validation: self.validation | rhs.validation,
            performance: self.performance | rhs.performance,
        }
    }
}

/// Error that can happen when creating a debug callback.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DebugCallbackCreationError {
    /// The `EXT_debug_utils` extension was not enabled.
    MissingExtension,
}

impl error::Error for DebugCallbackCreationError {}

impl fmt::Display for DebugCallbackCreationError {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(
            fmt,
            "{}",
            match *self {
                DebugCallbackCreationError::MissingExtension => {
                    "the `EXT_debug_utils` extension was not enabled"
                }
            }
        )
    }
}

impl From<Error> for DebugCallbackCreationError {
    #[inline]
    fn from(err: Error) -> DebugCallbackCreationError {
        panic!("unexpected error: {:?}", err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    #[test]
    fn ensure_sendable() {
        // It's useful to be able to initialize a DebugCallback on one thread
        // and keep it alive on another thread.
        let instance = instance!();
        let severity = MessageSeverity::none();
        let ty = MessageType::all();
        let callback = DebugCallback::new(&instance, severity, ty, |_| {});
        thread::spawn(move || {
            let _ = callback;
        });
    }
}
