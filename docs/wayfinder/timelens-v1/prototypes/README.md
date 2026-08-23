# Timelens throwaway interaction prototypes

These files answer design questions only. They are not production UI and must not be copied into the Slint application without a separate implementation review.

## Stacked timeline and middle-button range selection

Open `timeline-ui-prototype.html` directly, or serve this directory from the repository root:

```powershell
python -m http.server 4177 --bind 127.0.0.1 --directory docs\wayfinder\timelens-v1\prototypes
```

Then open `http://127.0.0.1:4177/timeline-ui-prototype.html?variant=A`.

Variant A is the user-selected direction. It keeps one full-width range-selection timeline at the top, a selected-period application list on the left, and one focused application detail surface in the center. Middle-drag the top timeline to change the range, click a left-side application to inspect it, and expand the numbered window rows when needed. Variants B and C remain available through `?variant=B` and `?variant=C` as discarded comparison evidence.
