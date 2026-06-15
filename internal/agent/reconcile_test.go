package agent

import "testing"

func testPane(hash string, focused bool) Pane {
	return Pane{PaneID: "%1", Target: "s:1.1", ContentHash: hash, WindowActive: focused}
}

func seedReconciler(status PaneStatus, hash string) *Reconciler {
	r := NewReconciler()
	r.prevContent["%1"] = hash
	r.prevStatuses["%1"] = status
	return r
}

func reconcileOne(r *Reconciler, p Pane) Pane {
	panes := []Pane{p}
	r.Reconcile(panes)
	return panes[0]
}

func TestFocusedIdleChangeStaysIdle(t *testing.T) {
	r := seedReconciler(StatusIdle, "a")

	p := reconcileOne(r, testPane("b", true))

	if p.Status != StatusIdle {
		t.Fatalf("status = %v, want idle", p.Status)
	}
}

func TestUnfocusedIdleChangeBecomesBusy(t *testing.T) {
	r := seedReconciler(StatusIdle, "a")

	p := reconcileOne(r, testPane("b", false))

	if p.Status != StatusBusy {
		t.Fatalf("status = %v, want busy", p.Status)
	}
}

func TestFocusedBusyChangeStaysBusy(t *testing.T) {
	r := seedReconciler(StatusBusy, "a")

	p := reconcileOne(r, testPane("b", true))

	if p.Status != StatusBusy {
		t.Fatalf("status = %v, want busy", p.Status)
	}
}

func TestBusySettlesToIdleWhenFocused(t *testing.T) {
	r := seedReconciler(StatusBusy, "a")

	p := reconcileOne(r, testPane("a", true))
	if p.Status != StatusBusy {
		t.Fatalf("after one stable sample status = %v, want busy", p.Status)
	}
	p = reconcileOne(r, testPane("a", true))
	if p.Status != StatusIdle {
		t.Fatalf("after two stable samples status = %v, want idle", p.Status)
	}
}

func TestBusySettlesToUnreadWhenUnfocused(t *testing.T) {
	r := seedReconciler(StatusBusy, "a")

	p := reconcileOne(r, testPane("a", false))
	if p.Status != StatusBusy {
		t.Fatalf("after one stable sample status = %v, want busy", p.Status)
	}
	p = reconcileOne(r, testPane("a", false))
	if p.Status != StatusUnread {
		t.Fatalf("after two stable samples status = %v, want unread", p.Status)
	}
}

func TestMovingContentKeepsBusyOnSameHash(t *testing.T) {
	r := seedReconciler(StatusBusy, "a")

	p := testPane("a", false)
	p.ContentMoving = true
	p = reconcileOne(r, p)
	if p.Status != StatusBusy {
		t.Fatalf("moving status = %v, want busy", p.Status)
	}
	if r.unchangedCount["%1"] != 0 {
		t.Fatalf("unchanged count = %d, want 0", r.unchangedCount["%1"])
	}
}

func TestManualUnreadSurvivesContentChange(t *testing.T) {
	r := seedReconciler(StatusIdle, "a")
	r.SetOverride("%1", StatusUnread, "a")

	p := reconcileOne(r, testPane("b", false))

	if p.Status != StatusUnread {
		t.Fatalf("status = %v, want unread", p.Status)
	}
	if !r.HasOverride("%1") {
		t.Fatal("manual unread override was cleared")
	}
}

func TestManualIdleClearsOnBackgroundOutput(t *testing.T) {
	r := seedReconciler(StatusUnread, "a")
	r.SetOverride("%1", StatusIdle, "a")

	p := reconcileOne(r, testPane("b", false))

	if p.Status != StatusBusy {
		t.Fatalf("status = %v, want busy", p.Status)
	}
	if r.HasOverride("%1") {
		t.Fatal("manual idle override should clear on background output")
	}
}

func TestManualIdleFocusedOutputStaysIdle(t *testing.T) {
	r := seedReconciler(StatusUnread, "a")
	r.SetOverride("%1", StatusIdle, "a")

	p := reconcileOne(r, testPane("b", true))

	if p.Status != StatusIdle {
		t.Fatalf("status = %v, want idle", p.Status)
	}
	if !r.HasOverride("%1") {
		t.Fatal("manual idle override should survive focused output")
	}
}
