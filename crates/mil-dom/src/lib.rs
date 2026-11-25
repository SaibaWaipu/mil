//! HTML/CSS document handling for `mil`.
//!
//! [`Page`] wraps a Blitz [`BaseDocument`](blitz_dom::BaseDocument) together
//! with the URL it was loaded from. It exposes the information a terminal
//! renderer needs — source text runs, backgrounds, links and hit testing — and
//! [`render_page`] composites that information into a
//! [`Surface`](mil_paint::Surface).

pub mod form;
pub mod page;
pub mod render;

pub use blitz_traits::events::HitResult;
pub use blitz_traits::shell::{ColorScheme, Viewport};
pub use form::{FieldKind, FormField, FormInfo};
pub use mil_paint::{PaintConfig, Rgb};
pub use page::{FillRect, LinkRegion, Page, TextRun, TextStyle};
pub use render::render_page;
