//! Sidebar departures widget. Subscribes to
//! [`hytte::services::departures::current()`] and renders the current
//! eight S-Bahn departures as a vertical list. Relative time labels
//! re-render on every emission of [`hytte::services::clock::now()`].
